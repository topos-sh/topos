//! `login <workspace-address>` / `logout [<workspace>|--all]` — SESSIONS: a session = user ×
//! workspace × installation, minted by the gh-style browser approval and carrying ONE
//! workspace-scoped bearer credential (`identity/sessions.json`). Further workspaces are further
//! logins; `logout` ends exactly the named session (revocable from both sides — the web sessions
//! pages carry the owner arms).
//!
//! **Login is the acceptance event.** The receipt states what connecting delivers (the profile's
//! delivered count); from then on delivery is silent, npm-style — no consent layer, no per-bundle
//! first-trust asks for workspace content. The flow is the RFC-8628 shape `follow` proved: card
//! fetch at the address origin → re-root onto the declared API base → `POST /v1/login/authorize`
//! toward the workspace's ADDRESS slug → a `0600` WAL carrying the flow code → poll
//! `POST /v1/login/token`; the granted poll carries the SESSION's credential (the promoted flow
//! code) and the login persists it as one session row. Re-invoking `login` RESUMES a pending flow.

use topos_types::PERSISTED_SCHEMA_VERSION;
use topos_types::results::{EnrollmentPending, LoginData, LogoutData};

use crate::ctx::Ctx;
use crate::enroll;
use crate::error::ClientError;
use crate::manifest::document::{EntryValue, ManifestScope};
use crate::plane::{DeliverySource, DeviceAuthPoll, LinkStatus};
use crate::sessions::{self, SESSION_ACTIVE, SESSION_PENDING, Session};

use super::connect::{EnrollConnect, machine_name, resolve_api_base};

/// Builds a delivery transport for `(base_url, credential, workspace_id)` — the acceptance
/// disclosure's best-effort delivered count rides it.
pub(crate) type SessionDeliveryConnect<'a> =
    dyn Fn(&str, &str, &str) -> Box<dyn DeliverySource> + 'a;

/// Builds a governance transport for `(base_url, credential)` — the logout self-revoke rides the
/// SESSION's own credential (never a device-global one).
pub(crate) type SessionRevokeConnect<'a> =
    dyn Fn(&str, &str) -> Box<dyn crate::plane::GovernanceSource> + 'a;

/// The network seams `login` needs.
pub(crate) struct LoginConnectors<'a> {
    pub enroll: &'a EnrollConnect<'a>,
    pub delivery: &'a SessionDeliveryConnect<'a>,
    /// The default WEB origin a bare workspace name dials (`TOPOS_PLANE_URL`, else the hosted
    /// default).
    pub web_origin: String,
}

/// A parsed login address: the web origin to card-fetch, the manifest-grammar HOST half, the
/// workspace ADDRESS slug (empty = the origin's own workspace), and an invitation token when the
/// address was an invite URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoginTarget {
    pub origin: String,
    pub host: String,
    pub workspace: String,
    pub invite_token: Option<String>,
}

/// Whether a segment reads as a HOST (dotted, or localhost, optionally with a port) rather than a
/// workspace slug — the same dot-disambiguation the reference grammar applies.
fn is_host_segment(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let bare = s.split(':').next().unwrap_or(s);
    (bare.contains('.') || bare == "localhost")
        && bare
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// Parse a `login` address by SHAPE (no network): a bare workspace name (the default server), a
/// bare server origin ("the workspace that origin addresses" — single-tenant installs), a
/// `<server>/<workspace>` pair, any of those as a pasted URL, or an invitation URL
/// (`<origin>[/<ws>]/invite/<token>` — the mail's terminal line verbatim).
pub(crate) fn parse_login_address(
    raw: &str,
    default_origin: &str,
) -> Result<LoginTarget, ClientError> {
    let token = raw.trim().trim_end_matches('/');
    if token.is_empty() {
        return Err(ClientError::InvalidArgument(
            "`topos login` needs a workspace address — `topos login <server>/<workspace>` (or a \
             bare workspace name for the default server)"
                .into(),
        ));
    }
    // Scheme: an explicit `http://` is honored (a local dev server); everything else is https.
    let (rest, scheme) = if let Some(r) = token.strip_prefix("https://") {
        (r, "https://")
    } else if let Some(r) = token.strip_prefix("http://") {
        (r, "http://")
    } else {
        (token, "https://")
    };
    // An invitation URL carries its token; the left half parses as an ordinary address.
    let (rest, invite_token) = match rest.split_once("/invite/") {
        Some((left, tok)) if !tok.is_empty() && !tok.contains('/') => {
            (left.trim_end_matches('/'), Some(tok.to_owned()))
        }
        _ => (rest, None),
    };
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let (origin, host, workspace) = match segments.as_slice() {
        [] => {
            return Err(ClientError::InvalidArgument(
                "the address names no server or workspace".into(),
            ));
        }
        [one] if is_host_segment(one) => {
            // A bare SERVER origin — the workspace the origin itself addresses.
            (format!("{scheme}{one}"), (*one).to_owned(), String::new())
        }
        [one] => {
            if !crate::resolve::is_workspace_name(one) {
                return Err(ClientError::InvalidArgument(format!(
                    "'{one}' is not a workspace name (lowercase letters, digits, hyphens) — or \
                     spell the full address: `topos login <server>/<workspace>`"
                )));
            }
            let origin = default_origin.trim_end_matches('/').to_owned();
            let host = origin
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .to_owned();
            (origin, host, (*one).to_owned())
        }
        [server, ws] if is_host_segment(server) => {
            if !crate::resolve::is_workspace_name(ws) {
                return Err(ClientError::InvalidArgument(format!(
                    "'{ws}' is not a workspace name (lowercase letters, digits, hyphens)"
                )));
            }
            (
                format!("{scheme}{server}"),
                (*server).to_owned(),
                (*ws).to_owned(),
            )
        }
        _ => {
            return Err(ClientError::InvalidArgument(
                "spell the address as `<server>/<workspace>` (or a bare workspace name for the \
                 default server)"
                    .into(),
            ));
        }
    };
    Ok(LoginTarget {
        origin,
        host,
        workspace,
        invite_token,
    })
}

/// `topos login [<workspace-address>]` — begin (no WAL) or resume (a pending session-login WAL)
/// the flow. The granted poll persists ONE session row; the receipt is the acceptance disclosure.
///
/// # Errors
/// [`ClientError::InvalidArgument`] on a malformed address (or none, with no flow to resume);
/// [`ClientError::Enrollment`] on a denied/expired flow or another verb's in-flight enrollment;
/// transport / io failures otherwise.
/// What the caller can do locally to complete this login.
///
/// Two facts that sound alike but must never be one flag (conflating them once shipped a login
/// that started device-bound while the terminal held a listener and suppressed the code — the
/// approval page then had nothing to pre-arm and the wait ran out the whole TTL):
/// `bind_loopback` is what a FRESH start declares the flow AS (write-once, server-side) — this
/// machine has a browser and a bound 127.0.0.1 listener, so the flow's credential is
/// unredeemable without the code that redirect delivers. `listening` is whether THIS POLL may
/// tolerate `awaiting_redirect` — true only once this invocation's own listener is armed; on the
/// first poll of a resume it is false, because an approval that predates this process handed its
/// code to a listener that no longer exists. `auth_code` carries the delivered code back in. A
/// client with none of the three is the classic device grant: the human types the short code and
/// the poll alone redeems.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Handoff<'a> {
    pub bind_loopback: bool,
    pub listening: bool,
    pub auth_code: Option<&'a str>,
}

pub(crate) fn login(
    ctx: &Ctx<'_>,
    connectors: &LoginConnectors<'_>,
    address: Option<&str>,
    handoff: Handoff<'_>,
) -> Result<LoginData, ClientError> {
    // A pending WAL first — re-invoking IS the resume; a foreign-owned flow refuses typed.
    if let Some(wal) = enroll::read_wal(ctx.fs, &ctx.layout)? {
        // `login <B>` while a flow for A is pending must NOT silently resume A — refuse typed
        // (finish or let it expire), so the receipt can never claim a workspace the person did
        // not just name. A matching address (or a bare `login`) resumes as before.
        if let Some(raw) = address
            && matches!(wal.intent, enroll::EnrollIntentDoc::Session)
        {
            let target = parse_login_address(raw, &connectors.web_origin)?;
            let same = target.host == wal.host
                && (target.workspace == wal.workspace_name || target.workspace.is_empty());
            if !same {
                return Err(ClientError::Enrollment(format!(
                    "a login for {}/{} is already pending — finish it (`topos login`) or let it \
                     expire before starting one for {}/{}",
                    wal.host, wal.workspace_name, target.host, target.workspace
                )));
            }
        }
        return match wal.intent {
            enroll::EnrollIntentDoc::Session => resume(ctx, connectors, &wal, handoff),
            enroll::EnrollIntentDoc::Retired => Err(ClientError::Enrollment(
                "a retired enrollment flow is on disk — it will be swept when it expires; start \
                 fresh with `topos login <workspace-address>`"
                    .into(),
            )),
        };
    }
    let Some(raw) = address else {
        return Err(ClientError::InvalidArgument(
            "`topos login` needs a workspace address — `topos login <server>/<workspace>` (or a \
             bare workspace name for the default server)"
                .into(),
        ));
    };
    let target = parse_login_address(raw, &connectors.web_origin)?;
    // The constant protocol card at the origin declares the API base (same-security re-root).
    let card = (connectors.enroll)(&target.origin).fetch_card(&target.origin)?;
    let base_url = resolve_api_base(&target.origin, &card.api_base_url)?;
    // THE START IS THE DECLARATION: the flow is bound, write-once, to how its credential may be
    // collected. `bind_loopback` — never `listening`, which is about polls — is what decides it.
    let start = (connectors.enroll)(&base_url).device_auth_start(
        &target.workspace,
        &machine_name(),
        target.invite_token.as_deref(),
        handoff.bind_loopback,
    )?;
    let now = i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX);
    let expires_at = now.saturating_add(
        i64::try_from(start.expires_in_secs.saturating_mul(1000)).unwrap_or(i64::MAX),
    );
    let wal = enroll::PendingEnrollment {
        schema_version: PERSISTED_SCHEMA_VERSION,
        base_url,
        host: target.host,
        workspace_name: target.workspace,
        intent: enroll::EnrollIntentDoc::Session,
        device_code: start.device_code,
        user_code: start.user_code,
        verification_uri: start.verification_uri,
        interval_secs: start.interval_secs,
        expires_at_millis: expires_at,
        loopback: handoff.bind_loopback,
        auth_code: None,
    };
    enroll::write_wal(ctx.fs, &ctx.layout, &wal)?;
    Ok(pending_data(&wal))
}

/// The pending (awaiting-browser-approval) receipt for a live WAL.
fn pending_data(wal: &enroll::PendingEnrollment) -> LoginData {
    LoginData {
        workspace_id: String::new(),
        name: wal.workspace_name.clone(),
        display_name: None,
        server: Some(wal.base_url.clone()),
        session_id: None,
        session_status: "awaiting-approval".to_owned(),
        delivered: None,
        delivered_names: Vec::new(),
        pending: Some(EnrollmentPending {
            verification_uri: wal.verification_uri.clone(),
            user_code: wal.user_code.clone(),
            expires_at: Some(super::connect::fmt_rfc3339_millis(wal.expires_at_millis)),
            interval_secs: Some(wal.interval_secs),
        }),
        currency: None,
        triggers: Vec::new(),
        manifest_note: None,
    }
}

/// Resume a live session-login WAL: poll once; granted ⇒ persist the SESSION row (the credential
/// is workspace-scoped), delete the WAL, arm the auto-update trigger, and disclose what connecting
/// delivers.
fn resume(
    ctx: &Ctx<'_>,
    connectors: &LoginConnectors<'_>,
    wal: &enroll::PendingEnrollment,
    handoff: Handoff<'_>,
) -> Result<LoginData, ClientError> {
    // A freshly-arrived authorization code is PERSISTED before it is spent: the human has
    // finished approving and the code is not re-derivable, so an interrupt between here and the
    // grant must not strand the login. Presenting it does not consume it server-side, so the
    // re-poll after a crash exchanges the same pair again.
    let wal = match handoff.auth_code {
        Some(code) if wal.auth_code.as_deref() != Some(code) => {
            let updated = enroll::PendingEnrollment {
                auth_code: Some(code.to_owned()),
                ..wal.clone()
            };
            enroll::write_wal(ctx.fs, &ctx.layout, &updated)?;
            updated
        }
        _ => wal.clone(),
    };
    let wal = &wal;
    let enroll_src = (connectors.enroll)(&wal.base_url);
    match enroll_src.device_auth_poll(&wal.device_code, wal.auth_code.as_deref())? {
        DeviceAuthPoll::Pending => Ok(pending_data(wal)),
        DeviceAuthPoll::AwaitingRedirect if handoff.listening => {
            // A live listener is still open — the human has approved and the redirect is on its
            // way. Reported as pending so the wait loop keeps its shape.
            Ok(pending_data(wal))
        }
        DeviceAuthPoll::AwaitingRedirect => {
            // No listener this time: the flow was approved, but the hand-off it needed went to a
            // listener that is gone (the process was interrupted, or the approval happened
            // somewhere that could not return it). The authorization code exists only in that
            // lost redirect, so this flow can never complete — and nothing was minted for it, so
            // there is nothing to clean up but the WAL. Clear it and say plainly to start again,
            // rather than re-polling a flow that will only ever answer this.
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::Enrollment(
                "that login was approved, but its approval could not be handed back to this \
                 machine — nothing was connected. Run `topos login <workspace-address>` again."
                    .into(),
            ))
        }
        DeviceAuthPoll::Denied => {
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::Enrollment(
                "the login was denied at the approval page".into(),
            ))
        }
        DeviceAuthPoll::Expired => {
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::Enrollment(
                "the login flow expired; start over with `topos login <workspace-address>`".into(),
            ))
        }
        DeviceAuthPoll::Granted(grant) => {
            let status = match grant.link_status {
                LinkStatus::Active => SESSION_ACTIVE,
                LinkStatus::Pending => SESSION_PENDING,
            };
            // The manifest-grammar host: the address host the human typed, else (a pre-field WAL)
            // the API base's own host.
            let host = if wal.host.is_empty() {
                wal.base_url
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .split('/')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            } else {
                wal.host.clone()
            };
            let session = Session {
                host,
                base_url: wal.base_url.clone(),
                workspace_id: grant.workspace.workspace_id.clone(),
                workspace_name: grant.workspace.name.clone(),
                display_name: grant.workspace.display_name.clone(),
                session_id: grant
                    .session_id
                    .clone()
                    .unwrap_or_else(|| grant.device_id.clone()),
                credential: grant.credential.clone(),
                status: status.to_owned(),
                logged_in_at: i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX),
            };
            sessions::upsert_session(ctx.fs, &ctx.layout, session.clone())?;
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            // Login is the trigger-arming moment for a receiving install (the acceptance event) —
            // best-effort, disclosed on the receipt, never a rolled-back login.
            let currency = Some(ctx.harness.install_currency_trigger());
            // The acceptance disclosure: what connecting delivers RIGHT NOW (best-effort; a
            // pending session delivers nothing until an owner approves).
            let snapshot = if status == SESSION_ACTIVE {
                (connectors.delivery)(
                    &session.base_url,
                    &session.credential,
                    &session.workspace_id,
                )
                .fetch_delivery(&session.workspace_id)
                .ok()
            } else {
                None
            };
            let delivered = snapshot.as_ref().map(|s| s.skills.len() as u64);
            let delivered_names = snapshot
                .as_ref()
                .map(|s| s.skills.iter().map(|d| d.name.clone()).collect())
                .unwrap_or_default();
            // The machine's own recipe, when it exists: this workspace's feed row joins it, so the
            // file keeps saying the whole truth about what lands here. No file = nothing to do
            // (an absent file already behaves as one feed row per connected workspace), and no
            // other workspace's rows are ever touched — a feed row someone deleted stays deleted.
            let manifest_note = record_feed_row(ctx, &session.host, &session.workspace_name);
            Ok(LoginData {
                workspace_id: session.workspace_id,
                name: session.workspace_name,
                display_name: Some(session.display_name),
                server: Some(session.base_url),
                session_id: Some(session.session_id),
                session_status: status.to_owned(),
                delivered,
                delivered_names,
                pending: None,
                currency,
                triggers: Vec::new(),
                manifest_note,
            })
        }
    }
}

/// Append THIS workspace's feed row to the machine's own `topos.toml` — when one exists. A file
/// that is absent is left absent (it already behaves exactly as one feed row per connected
/// workspace, and login is not the moment to take over a person's file); an existing file gets
/// exactly one row added, and nothing else in it is read as an instruction — a feed row someone
/// deleted for another workspace stays deleted. Best-effort: a refusal is DISCLOSED on the
/// receipt, never a failed login.
fn record_feed_row(ctx: &Ctx<'_>, host: &str, workspace: &str) -> Option<String> {
    let path = ctx.layout.home().join(crate::manifest::MANIFEST_FILE);
    // The same writer lock every manifest mutation takes — this append is a read-modify-write too.
    let _guard = super::manifest_edit::lock_manifest(ctx, &path).ok()?;
    let bytes = ctx.fs.read_opt(&path).ok()??;
    let text = String::from_utf8(bytes).ok()?;
    let reference = format!("{host}/{workspace}");
    let mut editor =
        match crate::manifest::document::ManifestEditor::open(&text, ManifestScope::Global) {
            Ok(e) => e,
            Err(e) => {
                return Some(format!(
                    "{} could not be read ({e}) — it was left untouched; fix it and add \
                     `\"{reference}\" = \"*\"` to take {workspace}'s whole feed",
                    path.display()
                ));
            }
        };
    if editor.row(&reference).is_some() {
        return Some(format!(
            "{} already takes {workspace}'s feed — unchanged",
            path.display()
        ));
    }
    if let Err(e) = editor.set_row(&reference, &EntryValue::Star) {
        return Some(format!(
            "{} was left untouched ({e}) — add `\"{reference}\" = \"*\"` there to take \
             {workspace}'s whole feed",
            path.display()
        ));
    }
    match editor.write(ctx.fs, &path) {
        Ok(()) => Some(format!(
            "added `\"{reference}\" = \"*\"` to {} — this machine takes whatever {workspace} \
             gives you; delete that line to take it bundle by bundle",
            path.display()
        )),
        Err(e) => Some(format!(
            "{} could not be written ({}) — it was left untouched",
            path.display(),
            e.detail()
        )),
    }
}

/// `topos logout [<workspace>] [--all]` — end session(s): the server-side revoke per session
/// (`DELETE /v1/session` under that session's OWN credential; the uniform 404 = already ended),
/// then the local row delete. The local sign-out proceeds regardless of the server outcome —
/// `server_revoked` reports it honestly. Skills, drafts, and manifests stay; `topos login
/// <address>` starts a fresh session.
///
/// # Errors
/// [`ClientError::Enrollment`] with no sessions; [`ClientError::WorkspaceSelection`] when several
/// sessions exist and none is named; an io/doc failure.
pub(crate) fn logout(
    ctx: &Ctx<'_>,
    revoke: &SessionRevokeConnect<'_>,
    workspace: Option<&str>,
    all: bool,
) -> Result<LogoutData, ClientError> {
    let all_sessions = sessions::read_sessions(ctx.fs, &ctx.layout)?;
    if all_sessions.sessions.is_empty() {
        return Err(ClientError::SessionRequired {
            address: "<workspace-address>".to_owned(),
            message: "not logged into any workspace; nothing to log out of".into(),
        });
    }
    let names: Vec<String> = all_sessions
        .sessions
        .iter()
        .map(|s| s.workspace_name.clone())
        .collect();
    let targets: Vec<Session> = if all {
        all_sessions.sessions.clone()
    } else if let Some(ws) = workspace {
        vec![
            all_sessions
                .find(ws)?
                .ok_or_else(|| {
                    ClientError::WorkspaceSelection(format!(
                        "not logged into workspace '{ws}'; logged-in workspaces: {}",
                        names.join(", ")
                    ))
                })?
                .clone(),
        ]
    } else {
        match all_sessions.sessions.as_slice() {
            [only] => vec![only.clone()],
            _ => {
                return Err(ClientError::WorkspaceSelection(format!(
                    "logged into multiple workspaces ({}); name one — `topos logout <workspace>` \
                     — or pass `--all`",
                    names.join(", ")
                )));
            }
        }
    };

    let mut ended = Vec::with_capacity(targets.len());
    let mut server_revoked = true;
    for s in &targets {
        // The server-side end, BEFORE the local delete (the revoke authenticates with the
        // session's own credential). Best-effort: unreachable never blocks the local sign-out —
        // `server_revoked` discloses it. The uniform 404 = the session is ALREADY gone
        // server-side (owner-ended, seat removed) — revoked-equivalent, never "failed".
        let ok = match (revoke)(&s.base_url, &s.credential).revoke_session() {
            Ok(()) => true,
            Err(ClientError::TargetNotFound { .. }) => true,
            Err(_) => false,
        };
        if !ok {
            server_revoked = false;
        }
        sessions::remove_session(ctx.fs, &ctx.layout, &s.host, &s.workspace_id)?;
        ended.push(s.workspace_name.clone());
    }
    Ok(LogoutData {
        ended,
        server_revoked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_grammar_table() {
        let d = "https://topos.sh";
        // A bare workspace name → the default server.
        assert_eq!(
            parse_login_address("acme", d).unwrap(),
            LoginTarget {
                origin: "https://topos.sh".into(),
                host: "topos.sh".into(),
                workspace: "acme".into(),
                invite_token: None
            }
        );
        // A bare SERVER origin (dotted) → the origin's own workspace (empty slug).
        assert_eq!(
            parse_login_address("topos.example.com", d).unwrap(),
            LoginTarget {
                origin: "https://topos.example.com".into(),
                host: "topos.example.com".into(),
                workspace: String::new(),
                invite_token: None
            }
        );
        // `<server>/<workspace>`, schemeless and as a pasted URL.
        for spelled in ["topos.example.com/eng", "https://topos.example.com/eng/"] {
            assert_eq!(
                parse_login_address(spelled, d).unwrap(),
                LoginTarget {
                    origin: "https://topos.example.com".into(),
                    host: "topos.example.com".into(),
                    workspace: "eng".into(),
                    invite_token: None
                },
                "{spelled}"
            );
        }
        // An explicit http:// origin is honored (a local dev server); a port survives.
        assert_eq!(
            parse_login_address("http://localhost:3000/acme", d).unwrap(),
            LoginTarget {
                origin: "http://localhost:3000".into(),
                host: "localhost:3000".into(),
                workspace: "acme".into(),
                invite_token: None
            }
        );
        // The invitation URL carries its token; the left half parses as an address.
        assert_eq!(
            parse_login_address("https://topos.sh/acme/invite/tok123", d).unwrap(),
            LoginTarget {
                origin: "https://topos.sh".into(),
                host: "topos.sh".into(),
                workspace: "acme".into(),
                invite_token: Some("tok123".into())
            }
        );
        assert_eq!(
            parse_login_address("https://topos.example.com/invite/tok9", d).unwrap(),
            LoginTarget {
                origin: "https://topos.example.com".into(),
                host: "topos.example.com".into(),
                workspace: String::new(),
                invite_token: Some("tok9".into())
            }
        );
    }

    #[test]
    fn malformed_addresses_refuse_typed() {
        let d = "https://topos.sh";
        for bad in ["", "  ", "Bad_Name", "a/b/c", "eng/acme"] {
            let err = parse_login_address(bad, d).unwrap_err();
            assert_eq!(err.code(), "INVALID_ARGUMENT", "{bad:?}");
        }
    }

    // ---- The flow over fakes (no HTTP). ----

    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    use crate::ctx::Ctx;
    use crate::fs_seam::RealFs;
    use crate::ids::{RealClock, RealIds};
    use crate::plane::{
        DeliverySnapshot, DeviceAuthStart, EnrollSource, EnrolledGrant, EnrolledWorkspace,
        GovernanceSource, PlaneError,
    };
    use crate::sidecar::Layout;
    use topos_harness::ClaudeCode;
    use topos_types::requests::WireProtocolCard;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("topos-login-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn with_ctx<R>(home: &Path, f: impl FnOnce(&Ctx<'_>) -> R) -> R {
        let fs = RealFs;
        let ids = RealIds;
        let clock = RealClock;
        let plane = crate::plane::InertPlane;
        let follow = crate::plane::InertFollow;
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
            roots: None,
        };
        f(&ctx)
    }

    /// A fake enrollment transport: the card declares an API base; the poll answers a scripted
    /// sequence (pending → granted); every start's BINDING declaration is recorded. State is
    /// Rc-shared so the connector can mint a fresh box per call over ONE script.
    #[derive(Clone, Default)]
    struct FakeEnroll {
        polls: std::rc::Rc<RefCell<Vec<DeviceAuthPoll>>>,
        /// The `loopback` declaration each `device_auth_start` carried — the write-once binding.
        starts: std::rc::Rc<RefCell<Vec<bool>>>,
    }
    impl FakeEnroll {
        fn scripted(polls: Vec<DeviceAuthPoll>) -> Self {
            Self {
                polls: std::rc::Rc::new(RefCell::new(polls)),
                starts: std::rc::Rc::default(),
            }
        }
    }
    impl EnrollSource for FakeEnroll {
        fn fetch_card(&self, _url: &str) -> Result<WireProtocolCard, ClientError> {
            Ok(WireProtocolCard {
                schema_version: 1,
                card: "topos-protocol-card".to_owned(),
                api_base_url: "https://topos.example.com/api".to_owned(),
            })
        }
        fn device_auth_start(
            &self,
            workspace: &str,
            _requested_name: &str,
            _invite_token: Option<&str>,
            loopback: bool,
        ) -> Result<DeviceAuthStart, ClientError> {
            assert_eq!(workspace, "eng");
            self.starts.borrow_mut().push(loopback);
            Ok(DeviceAuthStart {
                device_code: "flow-secret".to_owned(),
                user_code: "AB12-CD34".to_owned(),
                verification_uri: "https://topos.example.com/verify".to_owned(),
                expires_in_secs: 900,
                interval_secs: 5,
            })
        }
        fn device_auth_poll(
            &self,
            device_code: &str,
            _auth_code: Option<&str>,
        ) -> Result<DeviceAuthPoll, ClientError> {
            assert_eq!(device_code, "flow-secret");
            Ok(self.polls.borrow_mut().remove(0))
        }
    }

    struct EmptyDelivery;
    impl DeliverySource for EmptyDelivery {
        fn fetch_delivery(&self, _ws: &str) -> Result<DeliverySnapshot, PlaneError> {
            Ok(DeliverySnapshot {
                skills: Vec::new(),
                proposals_awaiting: 0,
                notices: Vec::new(),
                staleness_window_ms: 1000,
                link_status: LinkStatus::Active,
                declined: Vec::new(),
            })
        }
        fn report_applied(
            &self,
            _ws: &str,
            _applied: &[(String, [u8; 32])],
        ) -> Result<(), PlaneError> {
            Ok(())
        }
    }

    fn granted(status: LinkStatus) -> DeviceAuthPoll {
        DeviceAuthPoll::Granted(EnrolledGrant {
            credential: "sess-secret".to_owned(),
            device_id: "sn_1".to_owned(),
            session_id: Some("sn_1".to_owned()),
            workspace: EnrolledWorkspace {
                workspace_id: "w_eng".to_owned(),
                name: "eng".to_owned(),
                display_name: "Engineering".to_owned(),
            },
            hint: None,
            link_status: status,
        })
    }

    #[test]
    fn login_starts_pends_resumes_and_persists_the_session() {
        let home = scratch("flow");
        with_ctx(&home, |ctx| {
            let fake =
                FakeEnroll::scripted(vec![DeviceAuthPoll::Pending, granted(LinkStatus::Active)]);
            // The connector mints a fresh box per call, all sharing ONE poll script.
            let shim = {
                let fake = fake.clone();
                move |_base: &str| -> Box<dyn EnrollSource> { Box::new(fake.clone()) }
            };
            let delivery = |_b: &str, _c: &str, _w: &str| -> Box<dyn DeliverySource> {
                Box::new(EmptyDelivery)
            };
            let connectors = LoginConnectors {
                enroll: &shim,
                delivery: &delivery,
                web_origin: "https://topos.sh".to_owned(),
            };
            // START: writes the WAL, answers the pending disclosure.
            let start = login(
                ctx,
                &connectors,
                Some("topos.example.com/eng"),
                Handoff::default(),
            )
            .unwrap();
            assert!(start.pending.is_some());
            assert_eq!(start.session_status, "awaiting-approval");
            let wal = enroll::read_wal(ctx.fs, &ctx.layout).unwrap().unwrap();
            assert_eq!(wal.host, "topos.example.com");
            assert!(matches!(wal.intent, enroll::EnrollIntentDoc::Session));
            // RESUME 1: still pending (the fake's first scripted poll).
            let mid = login(ctx, &connectors, None, Handoff::default()).unwrap();
            assert!(mid.pending.is_some());
            // RESUME 2: granted — the session persists, the WAL dies, the receipt discloses.
            let done = login(ctx, &connectors, None, Handoff::default()).unwrap();
            assert!(done.pending.is_none());
            assert_eq!(done.session_status, "active");
            assert_eq!(done.workspace_id, "w_eng");
            assert_eq!(done.delivered, Some(0));
            assert!(done.currency.is_some(), "login arms the trigger");
            assert!(enroll::read_wal(ctx.fs, &ctx.layout).unwrap().is_none());
            let all = sessions::read_sessions(ctx.fs, &ctx.layout).unwrap();
            assert_eq!(all.sessions.len(), 1);
            let s = &all.sessions[0];
            assert_eq!(
                (
                    s.host.as_str(),
                    s.workspace_name.as_str(),
                    s.status.as_str()
                ),
                ("topos.example.com", "eng", SESSION_ACTIVE)
            );
            assert_eq!(s.credential, "sess-secret");
            assert_eq!(s.session_id, "sn_1");
        });
    }

    #[test]
    fn a_pending_session_grant_persists_pending_and_skips_the_count() {
        let home = scratch("pend");
        with_ctx(&home, |ctx| {
            let fake = FakeEnroll::scripted(vec![granted(LinkStatus::Pending)]);
            let shim = {
                let fake = fake.clone();
                move |_base: &str| -> Box<dyn EnrollSource> { Box::new(fake.clone()) }
            };
            let delivery = |_b: &str, _c: &str, _w: &str| -> Box<dyn DeliverySource> {
                panic!("a pending session must not dial delivery")
            };
            let connectors = LoginConnectors {
                enroll: &shim,
                delivery: &delivery,
                web_origin: "https://topos.sh".to_owned(),
            };
            login(
                ctx,
                &connectors,
                Some("topos.example.com/eng"),
                Handoff::default(),
            )
            .unwrap();
            let done = login(ctx, &connectors, None, Handoff::default()).unwrap();
            assert_eq!(done.session_status, "pending");
            assert!(done.delivered.is_none());
            let all = sessions::read_sessions(ctx.fs, &ctx.layout).unwrap();
            assert_eq!(all.sessions[0].status, SESSION_PENDING);
        });
    }

    #[test]
    fn a_fresh_start_declares_the_binding_the_handoff_holds() {
        let home = scratch("bind");
        with_ctx(&home, |ctx| {
            // With a listener held, the START must declare the flow loopback-bound — the
            // write-once fact the approval page's zero-typing card resolves on. (Declaring less
            // once shipped a device-bound flow behind a loopback terminal: the page could not
            // pre-arm, the suppressed code was nowhere, and the wait ran out the whole TTL.)
            let fake = FakeEnroll::scripted(Vec::new());
            let shim = {
                let fake = fake.clone();
                move |_base: &str| -> Box<dyn EnrollSource> { Box::new(fake.clone()) }
            };
            let delivery = |_b: &str, _c: &str, _w: &str| -> Box<dyn DeliverySource> {
                Box::new(EmptyDelivery)
            };
            let connectors = LoginConnectors {
                enroll: &shim,
                delivery: &delivery,
                web_origin: "https://topos.sh".to_owned(),
            };
            let start = login(
                ctx,
                &connectors,
                Some("topos.example.com/eng"),
                Handoff {
                    bind_loopback: true,
                    listening: false,
                    auth_code: None,
                },
            )
            .unwrap();
            assert!(start.pending.is_some());
            assert_eq!(fake.starts.borrow().as_slice(), &[true]);
            let wal = enroll::read_wal(ctx.fs, &ctx.layout).unwrap().unwrap();
            assert!(
                wal.loopback,
                "the WAL records the binding the start declared"
            );
            // Without a listener, the same start is the classic device grant.
            enroll::delete_wal(ctx.fs, &ctx.layout).unwrap();
            login(
                ctx,
                &connectors,
                Some("topos.example.com/eng"),
                Handoff::default(),
            )
            .unwrap();
            assert_eq!(fake.starts.borrow().as_slice(), &[true, false]);
            let wal = enroll::read_wal(ctx.fs, &ctx.layout).unwrap().unwrap();
            assert!(!wal.loopback);
        });
    }

    #[test]
    fn awaiting_redirect_is_terminal_unless_this_invocation_listens() {
        let home = scratch("redirect");
        with_ctx(&home, |ctx| {
            let fake = FakeEnroll::scripted(vec![
                DeviceAuthPoll::AwaitingRedirect,
                DeviceAuthPoll::AwaitingRedirect,
            ]);
            let shim = {
                let fake = fake.clone();
                move |_base: &str| -> Box<dyn EnrollSource> { Box::new(fake.clone()) }
            };
            let delivery = |_b: &str, _c: &str, _w: &str| -> Box<dyn DeliverySource> {
                Box::new(EmptyDelivery)
            };
            let connectors = LoginConnectors {
                enroll: &shim,
                delivery: &delivery,
                web_origin: "https://topos.sh".to_owned(),
            };
            login(
                ctx,
                &connectors,
                Some("topos.example.com/eng"),
                Handoff {
                    bind_loopback: true,
                    listening: false,
                    auth_code: None,
                },
            )
            .unwrap();
            // A LIVE listener tolerates the wait: the human approved, the redirect is coming.
            let mid = login(
                ctx,
                &connectors,
                None,
                Handoff {
                    bind_loopback: true,
                    listening: true,
                    auth_code: None,
                },
            )
            .unwrap();
            assert!(mid.pending.is_some());
            assert!(enroll::read_wal(ctx.fs, &ctx.layout).unwrap().is_some());
            // Without one, the code went to a listener that is gone — terminal, WAL cleared.
            let err = login(ctx, &connectors, None, Handoff::default()).unwrap_err();
            assert_eq!(err.code(), "LOGIN_FAILED");
            assert!(
                err.to_string().contains("could not be handed back"),
                "{err}"
            );
            assert!(enroll::read_wal(ctx.fs, &ctx.layout).unwrap().is_none());
        });
    }

    /// A fake governance transport recording session revokes (Rc-shared for per-call boxes).
    #[derive(Clone)]
    struct FakeRevoke {
        calls: std::rc::Rc<RefCell<Vec<String>>>,
        fail: bool,
    }
    impl GovernanceSource for FakeRevoke {
        fn invite(
            &self,
            _w: &str,
            _b: topos_types::requests::InvitationRequest,
        ) -> Result<topos_types::requests::InvitationData, ClientError> {
            unreachable!()
        }
        fn revoke_session(&self) -> Result<(), ClientError> {
            self.calls.borrow_mut().push("revoke".to_owned());
            if self.fail {
                // A TRANSPORT fault (the uniform 404 is revoked-equivalent, tested below).
                Err(ClientError::Plane("unreachable".to_owned()))
            } else {
                Ok(())
            }
        }
    }

    fn seed_session(ctx: &Ctx<'_>, ws: &str, name: &str) {
        sessions::upsert_session(
            ctx.fs,
            &ctx.layout,
            Session {
                host: "topos.example.com".to_owned(),
                base_url: "https://topos.example.com/api".to_owned(),
                workspace_id: ws.to_owned(),
                workspace_name: name.to_owned(),
                display_name: name.to_owned(),
                session_id: format!("sn_{ws}"),
                credential: format!("cred-{ws}"),
                status: SESSION_ACTIVE.to_owned(),
                logged_in_at: 1,
            },
        )
        .unwrap();
    }

    #[test]
    fn logout_selects_revokes_and_removes_locally() {
        let home = scratch("logout");
        with_ctx(&home, |ctx| {
            // Nothing to log out of → typed.
            let noop = |_b: &str, _c: &str| -> Box<dyn GovernanceSource> { unreachable!() };
            let err = logout(ctx, &noop, None, false).unwrap_err();
            assert_eq!(err.code(), "SESSION_REQUIRED");

            seed_session(ctx, "w_a", "acme");
            seed_session(ctx, "w_b", "beta");
            // Several sessions, none named → a typed selection, never a guess.
            let sel = logout(ctx, &noop, None, false).unwrap_err();
            assert!(sel.to_string().contains("--all"), "{sel}");

            // A named logout revokes THAT session and removes its row.
            let fake = FakeRevoke {
                calls: std::rc::Rc::new(RefCell::new(Vec::new())),
                fail: false,
            };
            let revoke = {
                let fake = fake.clone();
                move |_b: &str, _c: &str| -> Box<dyn GovernanceSource> { Box::new(fake.clone()) }
            };
            let out = logout(ctx, &revoke, Some("acme"), false).unwrap();
            assert_eq!(out.ended, vec!["acme"]);
            assert!(out.server_revoked);
            assert_eq!(fake.calls.borrow().len(), 1);
            let left = sessions::read_sessions(ctx.fs, &ctx.layout).unwrap();
            assert_eq!(left.sessions.len(), 1);
            assert_eq!(left.sessions[0].workspace_name, "beta");

            // `--all` with a server-side miss: the local sign-out proceeds; disclosed honestly.
            let failing = FakeRevoke {
                calls: std::rc::Rc::new(RefCell::new(Vec::new())),
                fail: true,
            };
            let revoke2 = {
                let failing = failing.clone();
                move |_b: &str, _c: &str| -> Box<dyn GovernanceSource> { Box::new(failing.clone()) }
            };
            let out = logout(ctx, &revoke2, None, true).unwrap();
            assert_eq!(out.ended, vec!["beta"]);
            assert!(!out.server_revoked);

            // The uniform 404 is REVOKED-EQUIVALENT (already ended server-side) — never a
            // "could not revoke" scare on the receipt.
            seed_session(ctx, "w_c", "gamma");
            struct GoneRevoke;
            impl GovernanceSource for GoneRevoke {
                fn invite(
                    &self,
                    _w: &str,
                    _b: topos_types::requests::InvitationRequest,
                ) -> Result<topos_types::requests::InvitationData, ClientError> {
                    unreachable!()
                }
                fn revoke_session(&self) -> Result<(), ClientError> {
                    Err(ClientError::TargetNotFound {
                        target: "session".to_owned(),
                    })
                }
            }
            let revoke3 =
                move |_b: &str, _c: &str| -> Box<dyn GovernanceSource> { Box::new(GoneRevoke) };
            let out = logout(ctx, &revoke3, Some("gamma"), false).unwrap();
            assert_eq!(out.ended, vec!["gamma"]);
            assert!(
                out.server_revoked,
                "the uniform 404 = already gone = revoked"
            );
            assert!(
                sessions::read_sessions(ctx.fs, &ctx.layout)
                    .unwrap()
                    .sessions
                    .is_empty()
            );
        });
    }
}
