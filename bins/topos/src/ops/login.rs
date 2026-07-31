//! `login [<address>]` / `logout [<workspace>|--all]` — SESSIONS: a session = user × workspace ×
//! installation, minted against a SERVER and carrying ONE workspace-scoped bearer credential
//! (`identity/sessions.json`). Further workspaces are further logins; `logout` ends exactly the
//! named session (revocable from both sides — the web sessions pages carry the owner arms).
//!
//! **Login is SERVER-first.** You authenticate against a server; WHICH workspace you join is
//! chosen — or created — by the signed-in human in the browser, where their seats are known. A
//! `login <workspace>` shortcut only PRESELECTS one for that chooser; the CLI never resolves a
//! workspace itself and this unauthenticated flow is never an existence oracle.
//!
//! **Login is the acceptance event.** The receipt states what connecting adopts (the delivered
//! count); from then on delivery is silent, npm-style — no consent layer, no per-bundle
//! first-trust asks for workspace content. The flow is the RFC-8628 shape: card fetch at the
//! address origin → re-root onto the declared API base → `POST /v1/login/authorize` → a `0600`
//! WAL carrying the flow code → poll `POST /v1/login/token`; the granted poll carries the
//! SESSION's credential (the promoted flow code) and the login persists it as one session row.
//! Re-invoking `login` RESUMES a pending flow. A machine already logged into the same server
//! takes the browser-free lane instead (`POST /v1/login/connect`) — seat standing is the trust
//! basis there, so the second workspace costs no ceremony.

use topos_types::PERSISTED_SCHEMA_VERSION;
use topos_types::results::{EnrollmentPending, LoginData, LogoutData};

use crate::ctx::Ctx;
use crate::enroll;
use crate::error::ClientError;
use crate::manifest::document::{EntryValue, ManifestScope};
use crate::plane::{DeliverySnapshot, DeliverySource, DeviceAuthPoll, LinkStatus, PlaneError};
use crate::sessions::{self, SESSION_ACTIVE, SESSION_ENDED, SESSION_PENDING, Session};

use super::connect::{EnrollConnect, machine_name, resolve_api_base};

/// Builds a delivery transport for `(base_url, credential, workspace_id)` — the acceptance
/// disclosure's best-effort delivered count rides it.
pub(crate) type SessionDeliveryConnect<'a> =
    dyn Fn(&str, &str, &str) -> Box<dyn DeliverySource> + 'a;

/// Builds a CREDENTIALED session-lane transport for `(base_url, credential)` — both acts that ride
/// a session's OWN credential (never a machine-global one): the logout self-revoke, and the
/// lane-side second connect that mints the next workspace's session without a browser.
pub(crate) type SessionLaneConnect<'a> =
    dyn Fn(&str, &str) -> Box<dyn crate::plane::GovernanceSource> + 'a;

/// The network seams `login` needs.
pub(crate) struct LoginConnectors<'a> {
    pub enroll: &'a EnrollConnect<'a>,
    pub delivery: &'a SessionDeliveryConnect<'a>,
    /// The lane a machine already logged into this server connects the next workspace over.
    pub lane: &'a SessionLaneConnect<'a>,
    /// The default WEB origin a bare `login` (and a bare workspace name) dials
    /// (`TOPOS_PLANE_URL`, else the hosted default).
    pub web_origin: String,
}

/// A parsed login address: the web origin to card-fetch, the manifest-grammar HOST half, the
/// workspace slug PRESELECTED for the browser chooser (empty = none — the human picks or creates
/// one there), and an invitation token when the address was an invite URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoginTarget {
    pub origin: String,
    pub host: String,
    pub preselect: String,
    pub invite_token: Option<String>,
}

impl LoginTarget {
    /// The bare `topos login` target: the default server, nothing preselected.
    fn origin_only(origin: &str) -> Self {
        let origin = origin.trim_end_matches('/').to_owned();
        Self {
            host: host_of(&origin),
            origin,
            preselect: String::new(),
            invite_token: None,
        }
    }
}

/// The manifest-grammar HOST half of an origin / API base (`https://topos.sh/api` → `topos.sh`).
fn host_of(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_owned()
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

/// Parse a `login` address by SHAPE (no network): a bare SERVER (anything dotted, or
/// `localhost[:port]`), a bare workspace name (a preselection on the default server), a
/// `<server>/<workspace>` pair, any of those as a pasted URL, or an invitation URL
/// (`<origin>[/<ws>]/invite/<token>` — the mail's terminal line verbatim). The dot is the whole
/// disambiguation, exactly as the reference grammar reads it.
pub(crate) fn parse_login_address(
    raw: &str,
    default_origin: &str,
) -> Result<LoginTarget, ClientError> {
    let token = raw.trim().trim_end_matches('/');
    if token.is_empty() {
        return Err(ClientError::InvalidArgument(
            "that address is empty — `topos login` takes a server (`topos.example.com`), a \
             workspace (`acme` or `topos.sh/acme`), or an invitation link; with no address at all \
             it logs into the default server"
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
    let (origin, host, preselect) = match segments.as_slice() {
        [] => {
            return Err(ClientError::InvalidArgument(
                "the address names no server or workspace".into(),
            ));
        }
        [one] if is_host_segment(one) => {
            // A bare SERVER — log into it and choose the workspace in the browser.
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
            let host = host_of(&origin);
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
                "spell the address as `<server>/<workspace>` (or a bare server, or a bare \
                 workspace name for the default server)"
                    .into(),
            ));
        }
    };
    Ok(LoginTarget {
        origin,
        host,
        preselect,
        invite_token,
    })
}

/// `topos login [<address>]` — the SERVER-first login, in order: resume a pending flow (or drop
/// it when this invocation names a different target), report a workspace this machine is already
/// connected to, mint a further workspace over the lane a live session on that server already
/// holds, and only then open the browser. The granted poll persists ONE session row; the receipt
/// is the acceptance disclosure.
///
/// `bind_loopback` is what a FRESH start declares the flow AS (write-once, server-side): this
/// machine has a browser and a bound 127.0.0.1 listener, so the approval page pre-arms from the
/// URL-borne challenge and its redirect wakes this process. It is an ACCELERATOR, not a second
/// secret — the poll completes the login either way, including when the human approves on their
/// phone.
///
/// # Errors
/// [`ClientError::InvalidArgument`] on a malformed address; [`ClientError::Enrollment`] on a
/// denied/expired flow or another verb's in-flight enrollment; transport / io failures otherwise.
pub(crate) fn login(
    ctx: &Ctx<'_>,
    connectors: &LoginConnectors<'_>,
    address: Option<&str>,
    bind_loopback: bool,
) -> Result<LoginData, ClientError> {
    let named = match address {
        Some(raw) => Some(parse_login_address(raw, &connectors.web_origin)?),
        None => None,
    };
    // A pending WAL first — re-invoking IS the resume.
    if let Some(wal) = enroll::read_wal(ctx.fs, &ctx.layout)? {
        match wal.intent {
            enroll::EnrollIntentDoc::Session => {
                let same = named
                    .as_ref()
                    .is_none_or(|t| t.host == wal.host && t.preselect == wal.preselect);
                if same {
                    return resume(ctx, connectors, &wal);
                }
                // `login <B>` while a flow toward A is pending: THE LATEST COMMAND WINS — refusing
                // would strand the person behind a ceremony they have already moved on from. The
                // old flow is SETTLED first, never merely dropped: it may already have minted.
                settle_abandoned(ctx, connectors, &wal)?;
            }
            enroll::EnrollIntentDoc::Retired => {
                return Err(ClientError::Enrollment(
                    "a retired enrollment flow is on disk — it will be swept when it expires; \
                     start fresh with `topos login <address>`"
                        .into(),
                ));
            }
        }
    }
    // No address at all: the default server, nothing preselected — the headline usage.
    let target = named.unwrap_or_else(|| LoginTarget::origin_only(&connectors.web_origin));

    // A named workspace this machine already reaches needs no ceremony at all.
    if !target.preselect.is_empty() {
        if let Some(data) = report_or_forget_connected(ctx, connectors, &target)? {
            return Ok(data);
        }
        if let Some(data) = connect_over_lane(ctx, connectors, &target)? {
            return Ok(data);
        }
    }

    // The constant protocol card at the origin declares the API base (same-security re-root).
    let card = (connectors.enroll)(&target.origin).fetch_card(&target.origin)?;
    let base_url = resolve_api_base(&target.origin, &card.api_base_url)?;
    // THE START IS THE DECLARATION: the flow records write-once how its approval is accelerated
    // back, which is what lets the approval page pre-arm itself and keep the code off this
    // terminal.
    let preselect = (!target.preselect.is_empty()).then_some(target.preselect.as_str());
    let start = (connectors.enroll)(&base_url).device_auth_start(
        &machine_name(),
        preselect,
        target.invite_token.as_deref(),
        bind_loopback,
    )?;
    let now = i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX);
    let expires_at = now.saturating_add(
        i64::try_from(start.expires_in_secs.saturating_mul(1000)).unwrap_or(i64::MAX),
    );
    let wal = enroll::PendingEnrollment {
        schema_version: PERSISTED_SCHEMA_VERSION,
        base_url,
        host: target.host,
        preselect: target.preselect,
        intent: enroll::EnrollIntentDoc::Session,
        device_code: start.device_code,
        user_code: start.user_code,
        verification_uri: start.verification_uri,
        interval_secs: start.interval_secs,
        expires_at_millis: expires_at,
        loopback: bind_loopback,
    };
    enroll::write_wal(ctx.fs, &ctx.layout, &wal)?;
    Ok(pending_data(&wal))
}

/// The pending (awaiting-browser-approval) receipt for a live WAL.
fn pending_data(wal: &enroll::PendingEnrollment) -> LoginData {
    LoginData {
        workspace_id: String::new(),
        host: if wal.host.is_empty() {
            host_of(&wal.base_url)
        } else {
            wal.host.clone()
        },
        name: wal.preselect.clone(),
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

/// The ALREADY-CONNECTED short-circuit: a live local session for this (host, workspace) answers
/// with the ordinary connected receipt and a freshly read delivered count — no browser, no
/// server-side mutation, nothing minted twice. The delivery read doubles as the liveness probe:
/// the uniform 404 means the session is gone server-side (ended, seat removed, workspace gone), so
/// the dead row is deleted and `None` sends the caller on to the ordinary login.
fn report_or_forget_connected(
    ctx: &Ctx<'_>,
    connectors: &LoginConnectors<'_>,
    target: &LoginTarget,
) -> Result<Option<LoginData>, ClientError> {
    let all = sessions::read_sessions(ctx.fs, &ctx.layout)?;
    let Some(session) = all
        .find_on_host(&target.host, &target.preselect)
        .filter(|s| s.status != SESSION_ENDED)
        .cloned()
    else {
        return Ok(None);
    };
    let snapshot = match (connectors.delivery)(
        &session.base_url,
        &session.credential,
        &session.workspace_id,
    )
    .fetch_delivery(&session.workspace_id)
    {
        Ok(snap) => Some(snap),
        Err(PlaneError::NotFound) => {
            sessions::remove_session(ctx.fs, &ctx.layout, &session.host, &session.workspace_id)?;
            return Ok(None);
        }
        // Unreachable / unreadable: the session stands, the count does not. The receipt omits the
        // parenthetical rather than guess a number.
        Err(_) => None,
    };
    let status = match snapshot.as_ref().map(|s| s.link_status) {
        Some(LinkStatus::Active) => SESSION_ACTIVE,
        Some(LinkStatus::Pending) => SESSION_PENDING,
        None => session.status.as_str(),
    };
    // The server just spoke: keep the local mirror honest (a pending session an owner approved
    // stops reading as pending here too). Best-effort — a receipt never fails on it.
    if status != session.status {
        let _ = sessions::set_session_status(
            ctx.fs,
            &ctx.layout,
            &session.host,
            &session.workspace_id,
            status,
        );
    }
    Ok(Some(connected_data(
        &session,
        status,
        snapshot.as_ref(),
        None,
        None,
    )))
}

/// The LANE-SIDE second connect: with no session for the named workspace but a live one on the
/// SAME server, the next workspace costs no browser — the acting session's credential asks the
/// server to mint against the person's seat. `None` = there is no lane, or the server answered
/// the uniform 404 (no seat there, or the acting session is itself gone): the browser flow then
/// shows what is actually true — an invitation, a create, or the honest miss.
fn connect_over_lane(
    ctx: &Ctx<'_>,
    connectors: &LoginConnectors<'_>,
    target: &LoginTarget,
) -> Result<Option<LoginData>, ClientError> {
    let all = sessions::read_sessions(ctx.fs, &ctx.layout)?;
    // The most recent ACTIVE session on this host is the one that acts — the freshest credential,
    // and the only standing the server accepts (a pending session carries no authority yet).
    let Some(actor) = all
        .sessions
        .iter()
        .filter(|s| s.host == target.host && s.status == SESSION_ACTIVE)
        .max_by_key(|s| s.logged_in_at)
    else {
        return Ok(None);
    };
    let minted = match (connectors.lane)(&actor.base_url, &actor.credential)
        .login_connect(&target.preselect, &machine_name())
    {
        Ok(minted) => minted,
        Err(ClientError::TargetNotFound { .. }) => return Ok(None),
        Err(e) => return Err(e),
    };
    let status = match minted.link_status {
        LinkStatus::Active => SESSION_ACTIVE,
        LinkStatus::Pending => SESSION_PENDING,
    };
    let session = Session {
        host: target.host.clone(),
        base_url: actor.base_url.clone(),
        workspace_id: minted.workspace.workspace_id,
        workspace_name: minted.workspace.name,
        display_name: minted.workspace.display_name,
        session_id: minted.session_id,
        credential: minted.credential,
        status: status.to_owned(),
        logged_in_at: i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX),
    };
    sessions::upsert_session(ctx.fs, &ctx.layout, session.clone())?;
    Ok(Some(connected_receipt(ctx, connectors, &session)))
}

/// Resume a live session-login WAL: poll once; granted ⇒ persist the SESSION row (the credential
/// is workspace-scoped), delete the WAL, arm the auto-update trigger, and disclose what connecting
/// adopts. The poll is the ONE completion mechanism — an approval given on another device settles
/// here exactly like one given in this machine's own browser.
fn resume(
    ctx: &Ctx<'_>,
    connectors: &LoginConnectors<'_>,
    wal: &enroll::PendingEnrollment,
) -> Result<LoginData, ClientError> {
    let enroll_src = (connectors.enroll)(&wal.base_url);
    match enroll_src.device_auth_poll(&wal.device_code)? {
        DeviceAuthPoll::Pending => Ok(pending_data(wal)),
        DeviceAuthPoll::Denied => {
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::Enrollment(
                "the login was denied at the approval page".into(),
            ))
        }
        DeviceAuthPoll::Expired => {
            enroll::delete_wal(ctx.fs, &ctx.layout)?;
            Err(ClientError::Enrollment(
                "the login flow expired; start over with `topos login`".into(),
            ))
        }
        DeviceAuthPoll::Granted(grant) => {
            let session = persist_grant(ctx, wal, grant)?;
            Ok(connected_receipt(ctx, connectors, &session))
        }
    }
}

/// Persist a GRANTED flow's session and retire its WAL — the one place a grant becomes a durable
/// session row, so the ordinary resume and the settle of an abandoned flow record it identically.
/// The row lands BEFORE the WAL dies: the flow code in that WAL is the only handle on the minted
/// credential, so it is discarded only once the credential is safely on disk (a crash between the
/// two re-polls the same grant, which answers the same session).
fn persist_grant(
    ctx: &Ctx<'_>,
    wal: &enroll::PendingEnrollment,
    grant: crate::plane::EnrolledGrant,
) -> Result<Session, ClientError> {
    let status = match grant.link_status {
        LinkStatus::Active => SESSION_ACTIVE,
        LinkStatus::Pending => SESSION_PENDING,
    };
    // The manifest-grammar host: the address host the human typed, else (a pre-field WAL) the API
    // base's own host.
    let host = if wal.host.is_empty() {
        host_of(&wal.base_url)
    } else {
        wal.host.clone()
    };
    let session = Session {
        host,
        base_url: wal.base_url.clone(),
        workspace_id: grant.workspace.workspace_id,
        workspace_name: grant.workspace.name,
        display_name: grant.workspace.display_name,
        session_id: grant.session_id,
        credential: grant.credential,
        status: status.to_owned(),
        logged_in_at: i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX),
    };
    sessions::upsert_session(ctx.fs, &ctx.layout, session.clone())?;
    enroll::delete_wal(ctx.fs, &ctx.layout)?;
    Ok(session)
}

/// SETTLE an abandoned flow before its WAL is discarded, so `login <B>` can never throw away a
/// session that already exists. The exchange can COMMIT server-side while its response is lost —
/// the flow code in the WAL is then the only copy of a minted credential, and deleting it would
/// strand a live session nobody can reach. So: one best-effort poll. Granted ⇒ keep that session
/// through the whole granted tail, exactly as if this command had completed it (it shows up in
/// `topos status` and its skills arrive on the next update); the receipt that prints is still the
/// NEW login's, because the latest command is what the person asked for. Anything else — pending,
/// denied, expired, or an unreachable server — is nothing to keep: an unpolled flow can never have
/// minted, and a dead old server must not block the login just asked for.
fn settle_abandoned(
    ctx: &Ctx<'_>,
    connectors: &LoginConnectors<'_>,
    wal: &enroll::PendingEnrollment,
) -> Result<(), ClientError> {
    match (connectors.enroll)(&wal.base_url).device_auth_poll(&wal.device_code) {
        Ok(DeviceAuthPoll::Granted(grant)) => {
            // An io failure here propagates with the WAL intact — the next run settles it again
            // rather than losing the credential quietly.
            let session = persist_grant(ctx, wal, grant)?;
            let _settled = connected_receipt(ctx, connectors, &session);
            Ok(())
        }
        _ => enroll::delete_wal(ctx.fs, &ctx.layout),
    }
}

/// The shared tail of a login that just MINTED a session (the browser grant and the lane-side
/// connect alike): arm the auto-update trigger, read the acceptance disclosure, join this
/// workspace's feed row, and build the receipt. Every step is best-effort — a login is never
/// rolled back because a follow-up read failed; the receipt says so instead.
fn connected_receipt(
    ctx: &Ctx<'_>,
    connectors: &LoginConnectors<'_>,
    session: &Session,
) -> LoginData {
    // Login is the trigger-arming moment for a receiving install (the acceptance event).
    let currency = Some(ctx.harness.install_currency_trigger());
    // What connecting adopts RIGHT NOW (a pending session adopts nothing until an owner approves,
    // so it is never dialed).
    let snapshot = if session.status == SESSION_ACTIVE {
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
    // The machine's own recipe, when it exists: this workspace's feed row joins it, so the file
    // keeps saying the whole truth about what lands here. No file = nothing to do (an absent file
    // already behaves as one feed row per connected workspace), and no other workspace's rows are
    // ever touched — a feed row someone deleted stays deleted.
    let manifest_note = record_feed_row(ctx, &session.host, &session.workspace_name);
    connected_data(
        session,
        &session.status,
        snapshot.as_ref(),
        currency,
        manifest_note,
    )
}

/// The ONE connected-session payload — the browser grant, the lane-side connect, and the
/// already-connected report all render from this shape (`currency`/`manifest_note` are the two
/// things only a fresh mint has done).
fn connected_data(
    session: &Session,
    status: &str,
    snapshot: Option<&DeliverySnapshot>,
    currency: Option<topos_types::TriggerReport>,
    manifest_note: Option<String>,
) -> LoginData {
    LoginData {
        workspace_id: session.workspace_id.clone(),
        host: session.host.clone(),
        name: session.workspace_name.clone(),
        display_name: Some(session.display_name.clone()),
        server: Some(session.base_url.clone()),
        session_id: Some(session.session_id.clone()),
        session_status: status.to_owned(),
        delivered: snapshot.map(|s| s.skills.len() as u64),
        delivered_names: snapshot
            .map(|s| s.skills.iter().map(|d| d.name.clone()).collect())
            .unwrap_or_default(),
        pending: None,
        currency,
        triggers: Vec::new(),
        manifest_note,
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
    revoke: &SessionLaneConnect<'_>,
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
        // No address at all → the default server, nothing preselected (the headline usage).
        assert_eq!(
            LoginTarget::origin_only(d),
            LoginTarget {
                origin: "https://topos.sh".into(),
                host: "topos.sh".into(),
                preselect: String::new(),
                invite_token: None
            }
        );
        // One token WITHOUT a dot is a workspace on the default server — a PRESELECT shortcut.
        assert_eq!(
            parse_login_address("acme", d).unwrap(),
            LoginTarget {
                origin: "https://topos.sh".into(),
                host: "topos.sh".into(),
                preselect: "acme".into(),
                invite_token: None
            }
        );
        // One token WITH a dot (or localhost) is a SERVER — log in there, choose in the browser.
        for spelled in ["topos.example.com", "https://topos.example.com/"] {
            assert_eq!(
                parse_login_address(spelled, d).unwrap(),
                LoginTarget {
                    origin: "https://topos.example.com".into(),
                    host: "topos.example.com".into(),
                    preselect: String::new(),
                    invite_token: None
                },
                "{spelled}"
            );
        }
        assert_eq!(
            parse_login_address("http://localhost:3000", d).unwrap(),
            LoginTarget {
                origin: "http://localhost:3000".into(),
                host: "localhost:3000".into(),
                preselect: String::new(),
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
                    preselect: "eng".into(),
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
                preselect: "acme".into(),
                invite_token: None
            }
        );
        // The invitation URL carries its token; the left half parses as an address.
        assert_eq!(
            parse_login_address("https://topos.sh/acme/invite/tok123", d).unwrap(),
            LoginTarget {
                origin: "https://topos.sh".into(),
                host: "topos.sh".into(),
                preselect: "acme".into(),
                invite_token: Some("tok123".into())
            }
        );
        assert_eq!(
            parse_login_address("https://topos.example.com/invite/tok9", d).unwrap(),
            LoginTarget {
                origin: "https://topos.example.com".into(),
                host: "topos.example.com".into(),
                preselect: String::new(),
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
    use std::rc::Rc;

    use crate::ctx::Ctx;
    use crate::fs_seam::RealFs;
    use crate::ids::{RealClock, RealIds};
    use crate::plane::{
        ConnectedSession, DeliverySkill, DeviceAuthStart, EnrollSource, EnrolledGrant,
        EnrolledWorkspace, GovernanceSource,
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

    /// What one `device_auth_start` declared: the preselection the browser chooser receives (or
    /// none), and the write-once loopback binding.
    type StartRecord = (Option<String>, bool);

    /// A fake login transport: the card declares an API base; the poll answers a scripted
    /// sequence (pending → granted); every start's declaration is recorded. State is Rc-shared so
    /// the connector can mint a fresh box per call over ONE script.
    #[derive(Clone, Default)]
    struct FakeEnroll {
        polls: Rc<RefCell<Vec<DeviceAuthPoll>>>,
        /// When set, every poll answers this transport fault instead of the script (an old
        /// server gone unreachable).
        poll_fault: Rc<RefCell<Option<String>>>,
        starts: Rc<RefCell<Vec<StartRecord>>>,
    }
    impl FakeEnroll {
        fn scripted(polls: Vec<DeviceAuthPoll>) -> Self {
            Self {
                polls: Rc::new(RefCell::new(polls)),
                poll_fault: Rc::default(),
                starts: Rc::default(),
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
            _requested_name: &str,
            preselect: Option<&str>,
            _invite_token: Option<&str>,
            loopback: bool,
        ) -> Result<DeviceAuthStart, ClientError> {
            self.starts
                .borrow_mut()
                .push((preselect.map(str::to_owned), loopback));
            Ok(DeviceAuthStart {
                device_code: "flow-secret".to_owned(),
                user_code: "AB12-CD34".to_owned(),
                verification_uri: "https://topos.example.com/verify".to_owned(),
                expires_in_secs: 900,
                interval_secs: 5,
            })
        }
        fn device_auth_poll(&self, device_code: &str) -> Result<DeviceAuthPoll, ClientError> {
            assert_eq!(device_code, "flow-secret");
            if let Some(fault) = self.poll_fault.borrow().as_ref() {
                return Err(ClientError::Plane(fault.clone()));
            }
            Ok(self.polls.borrow_mut().remove(0))
        }
    }

    /// A fake delivery transport: a scripted answer (a named set, or the uniform 404) and a call
    /// count — the acceptance disclosure and the already-connected liveness probe both ride it.
    #[derive(Clone, Default)]
    struct FakeDelivery {
        names: Vec<String>,
        not_found: bool,
        pending: bool,
        calls: Rc<RefCell<usize>>,
    }
    impl DeliverySource for FakeDelivery {
        fn fetch_delivery(&self, _ws: &str) -> Result<DeliverySnapshot, PlaneError> {
            *self.calls.borrow_mut() += 1;
            if self.not_found {
                return Err(PlaneError::NotFound);
            }
            Ok(DeliverySnapshot {
                skills: self
                    .names
                    .iter()
                    .map(|n| DeliverySkill {
                        skill_id: format!("sk_{n}"),
                        name: n.clone(),
                        review_required: false,
                        version_id: [0u8; 32],
                        generation: 1,
                        bundle_digest: [0u8; 32],
                        via_channels: Vec::new(),
                        assigned_by: None,
                        picked: false,
                    })
                    .collect(),
                proposals_awaiting: 0,
                notices: Vec::new(),
                staleness_window_ms: 1000,
                link_status: if self.pending {
                    LinkStatus::Pending
                } else {
                    LinkStatus::Active
                },
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

    /// A fake session lane: one scripted `login/connect` answer, and the `(workspace, credential)`
    /// pairs it was asked with — the acting credential is the whole authority, so the test reads it.
    #[derive(Clone)]
    struct FakeLane {
        answer: Rc<RefCell<Option<Result<ConnectedSession, ClientError>>>>,
        calls: Rc<RefCell<Vec<(String, String)>>>,
        credential: String,
    }
    impl GovernanceSource for FakeLane {
        fn invite(
            &self,
            _w: &str,
            _b: topos_types::requests::InvitationRequest,
        ) -> Result<topos_types::requests::InvitationData, ClientError> {
            unreachable!()
        }
        fn login_connect(
            &self,
            workspace: &str,
            _requested_name: &str,
        ) -> Result<ConnectedSession, ClientError> {
            self.calls
                .borrow_mut()
                .push((workspace.to_owned(), self.credential.clone()));
            self.answer
                .borrow_mut()
                .take()
                .unwrap_or_else(|| panic!("the lane was dialed with no scripted answer"))
        }
    }

    fn granted(status: LinkStatus) -> DeviceAuthPoll {
        DeviceAuthPoll::Granted(EnrolledGrant {
            credential: "sess-secret".to_owned(),
            session_id: "sn_1".to_owned(),
            workspace: EnrolledWorkspace {
                workspace_id: "w_eng".to_owned(),
                name: "eng".to_owned(),
                display_name: "Engineering".to_owned(),
            },
            hint: None,
            link_status: status,
        })
    }

    /// The whole connector set over one script: the enrollment fake, one delivery answer for
    /// every dial, and a lane that panics unless a test scripts it.
    struct Rig {
        enroll: FakeEnroll,
        delivery: FakeDelivery,
        lane_answer: Rc<RefCell<Option<Result<ConnectedSession, ClientError>>>>,
        lane_calls: Rc<RefCell<Vec<(String, String)>>>,
    }

    impl Rig {
        fn new(polls: Vec<DeviceAuthPoll>) -> Self {
            Self {
                enroll: FakeEnroll::scripted(polls),
                delivery: FakeDelivery::default(),
                lane_answer: Rc::default(),
                lane_calls: Rc::default(),
            }
        }

        fn delivering(mut self, names: &[&str]) -> Self {
            self.delivery.names = names.iter().map(|n| (*n).to_owned()).collect();
            self
        }

        /// Run `f` with the connectors wired over this rig's fakes.
        fn with<R>(&self, f: impl FnOnce(&LoginConnectors<'_>) -> R) -> R {
            let enroll = {
                let fake = self.enroll.clone();
                move |_base: &str| -> Box<dyn EnrollSource> { Box::new(fake.clone()) }
            };
            let delivery = {
                let fake = self.delivery.clone();
                move |_b: &str, _c: &str, _w: &str| -> Box<dyn DeliverySource> {
                    Box::new(fake.clone())
                }
            };
            let lane = {
                let (answer, calls) = (Rc::clone(&self.lane_answer), Rc::clone(&self.lane_calls));
                move |_b: &str, cred: &str| -> Box<dyn GovernanceSource> {
                    Box::new(FakeLane {
                        answer: Rc::clone(&answer),
                        calls: Rc::clone(&calls),
                        credential: cred.to_owned(),
                    })
                }
            };
            f(&LoginConnectors {
                enroll: &enroll,
                delivery: &delivery,
                lane: &lane,
                web_origin: "https://topos.sh".to_owned(),
            })
        }
    }

    #[test]
    fn login_starts_pends_resumes_and_persists_the_session() {
        let home = scratch("flow");
        with_ctx(&home, |ctx| {
            let rig = Rig::new(vec![DeviceAuthPoll::Pending, granted(LinkStatus::Active)]);
            rig.with(|connectors| {
                // START: writes the WAL, answers the pending disclosure — and the named workspace
                // rides the start as the browser chooser's PRESELECTION.
                let start = login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap();
                assert!(start.pending.is_some());
                assert_eq!(start.session_status, "awaiting-approval");
                assert_eq!(start.host, "topos.example.com");
                assert_eq!(
                    rig.enroll.starts.borrow().as_slice(),
                    &[(Some("eng".to_owned()), false)]
                );
                let wal = enroll::read_wal(ctx.fs, &ctx.layout).unwrap().unwrap();
                assert_eq!(wal.host, "topos.example.com");
                assert_eq!(wal.preselect, "eng");
                assert!(matches!(wal.intent, enroll::EnrollIntentDoc::Session));
                // RESUME 1: still pending (the fake's first scripted poll).
                let mid = login(ctx, connectors, None, false).unwrap();
                assert!(mid.pending.is_some());
                // RESUME 2: granted — the session persists, the WAL dies, the receipt discloses.
                let done = login(ctx, connectors, None, false).unwrap();
                assert!(done.pending.is_none());
                assert_eq!(done.session_status, "active");
                assert_eq!(done.workspace_id, "w_eng");
                assert_eq!(
                    (done.host.as_str(), done.name.as_str()),
                    ("topos.example.com", "eng")
                );
                assert_eq!(done.delivered, Some(0));
                assert!(done.currency.is_some(), "login arms the trigger");
                assert!(enroll::read_wal(ctx.fs, &ctx.layout).unwrap().is_none());
            });
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
    fn a_bare_login_runs_against_the_default_server_with_nothing_preselected() {
        let home = scratch("bare");
        with_ctx(&home, |ctx| {
            let rig = Rig::new(Vec::new());
            rig.with(|connectors| {
                let start = login(ctx, connectors, None, false).unwrap();
                assert!(
                    start.pending.is_some(),
                    "bare login is legal — no WAL needed"
                );
                assert_eq!(start.name, "", "no workspace is named before the browser");
                assert_eq!(start.host, "topos.sh", "the default server");
                // NOTHING is preselected: the chooser is the browser's, not the CLI's.
                assert_eq!(rig.enroll.starts.borrow().as_slice(), &[(None, false)]);
                let wal = enroll::read_wal(ctx.fs, &ctx.layout).unwrap().unwrap();
                assert_eq!(wal.host, "topos.sh");
                assert!(wal.preselect.is_empty());
            });
        });
    }

    #[test]
    fn a_restart_settles_a_granted_flow_before_dropping_it() {
        let home = scratch("restart-settle");
        with_ctx(&home, |ctx| {
            // The exchange can COMMIT server-side while its answer is lost — the WAL's flow code
            // is then the only handle on a minted credential. A restart SETTLES the old flow
            // first; binning it would strand a live session nobody on this machine can reach.
            let rig = Rig::new(vec![granted(LinkStatus::Active)]);
            rig.with(|connectors| {
                login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap();
                let next = login(ctx, connectors, Some("other.example.com/ops"), false).unwrap();
                // THE LATEST COMMAND WINS: the receipt that prints is the new login's.
                assert!(next.pending.is_some());
                assert_eq!(
                    (next.host.as_str(), next.name.as_str()),
                    ("other.example.com", "ops")
                );
                let wal = enroll::read_wal(ctx.fs, &ctx.layout).unwrap().unwrap();
                assert_eq!(
                    (wal.host.as_str(), wal.preselect.as_str()),
                    ("other.example.com", "ops")
                );
            });
            // The settled session is kept WHOLE — indistinguishable from one this command
            // completed, which is what `topos status` and the next update will act on.
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
        });
    }

    #[test]
    fn a_restart_drops_a_flow_that_settled_nothing() {
        // Pending / denied / expired / an unreachable old server: an unpolled flow can never have
        // minted, and a dead server must never block the login just asked for.
        for (tag, script, fault) in [
            ("pending", vec![DeviceAuthPoll::Pending], None),
            ("denied", vec![DeviceAuthPoll::Denied], None),
            ("expired", vec![DeviceAuthPoll::Expired], None),
            ("unreachable", Vec::new(), Some("unreachable")),
        ] {
            let home = scratch(&format!("restart-{tag}"));
            with_ctx(&home, |ctx| {
                let rig = Rig::new(script);
                *rig.enroll.poll_fault.borrow_mut() = fault.map(str::to_owned);
                rig.with(|connectors| {
                    login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap();
                    let next =
                        login(ctx, connectors, Some("topos.example.com/ops"), false).unwrap();
                    assert!(next.pending.is_some(), "{tag}");
                    assert_eq!(next.name, "ops", "{tag}");
                    let wal = enroll::read_wal(ctx.fs, &ctx.layout).unwrap().unwrap();
                    assert_eq!(wal.preselect, "ops", "{tag}");
                    assert_eq!(
                        rig.enroll.starts.borrow().as_slice(),
                        &[
                            (Some("eng".to_owned()), false),
                            (Some("ops".to_owned()), false)
                        ],
                        "{tag}"
                    );
                });
                assert!(
                    sessions::read_sessions(ctx.fs, &ctx.layout)
                        .unwrap()
                        .sessions
                        .is_empty(),
                    "{tag}: nothing settled, so nothing is kept"
                );
            });
        }
    }

    #[test]
    fn an_already_connected_workspace_answers_without_a_ceremony() {
        let home = scratch("connected");
        with_ctx(&home, |ctx| {
            seed_session(ctx, "w_eng", "eng");
            let rig = Rig::new(Vec::new()).delivering(&["deploy", "code-review"]);
            rig.with(|connectors| {
                let out = login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap();
                assert!(out.pending.is_none(), "no browser, no flow");
                assert_eq!(out.session_status, "active");
                assert_eq!(out.workspace_id, "w_eng");
                // A LIVE count, read now — the receipt states what the session adopts today.
                assert_eq!(out.delivered, Some(2));
                assert_eq!(out.delivered_names, vec!["deploy", "code-review"]);
                // Nothing was minted, armed, or written: reporting is not re-accepting.
                assert!(out.currency.is_none());
                assert!(out.manifest_note.is_none());
                assert!(rig.enroll.starts.borrow().is_empty(), "no flow was started");
                assert!(rig.lane_calls.borrow().is_empty(), "no lane connect either");
            });
        });
    }

    #[test]
    fn a_dead_session_is_forgotten_and_the_login_runs_on() {
        let home = scratch("dead");
        with_ctx(&home, |ctx| {
            seed_session(ctx, "w_eng", "eng");
            let mut rig = Rig::new(Vec::new());
            // The uniform 404: ended, seat removed, or the workspace is gone — indistinguishable
            // by design, and all of them mean the local row is a lie.
            rig.delivery.not_found = true;
            rig.with(|connectors| {
                let out = login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap();
                assert!(out.pending.is_some(), "the ordinary login runs");
                assert_eq!(
                    rig.enroll.starts.borrow().as_slice(),
                    &[(Some("eng".to_owned()), false)]
                );
            });
            assert!(
                sessions::read_sessions(ctx.fs, &ctx.layout)
                    .unwrap()
                    .sessions
                    .is_empty(),
                "the dead row is deleted, not carried"
            );
        });
    }

    #[test]
    fn a_second_workspace_on_the_same_server_needs_no_browser() {
        let home = scratch("lane");
        with_ctx(&home, |ctx| {
            seed_session(ctx, "w_eng", "eng");
            let rig = Rig::new(Vec::new()).delivering(&["deploy"]);
            *rig.lane_answer.borrow_mut() = Some(Ok(ConnectedSession {
                credential: "sess-ops".to_owned(),
                session_id: "sn_ops".to_owned(),
                workspace: EnrolledWorkspace {
                    workspace_id: "w_ops".to_owned(),
                    name: "ops".to_owned(),
                    display_name: "Operations".to_owned(),
                },
                link_status: LinkStatus::Active,
            }));
            rig.with(|connectors| {
                let out = login(ctx, connectors, Some("topos.example.com/ops"), false).unwrap();
                assert!(out.pending.is_none(), "no browser for the second workspace");
                assert_eq!(out.workspace_id, "w_ops");
                assert_eq!(out.session_status, "active");
                assert_eq!(out.delivered, Some(1));
                assert!(out.currency.is_some(), "a fresh session arms the trigger");
                assert!(rig.enroll.starts.borrow().is_empty(), "no flow was started");
                // The ACTING credential is the live session's own — seat standing, not a secret
                // this machine invented.
                assert_eq!(
                    rig.lane_calls.borrow().as_slice(),
                    &[("ops".to_owned(), "cred-w_eng".to_owned())]
                );
            });
            let all = sessions::read_sessions(ctx.fs, &ctx.layout).unwrap();
            assert_eq!(all.sessions.len(), 2, "both sessions stand");
            let ops = all.find_on_host("topos.example.com", "ops").unwrap();
            assert_eq!(ops.credential, "sess-ops");
            assert_eq!(ops.session_id, "sn_ops");
        });
    }

    #[test]
    fn a_lane_miss_falls_through_to_the_browser() {
        let home = scratch("lane-miss");
        with_ctx(&home, |ctx| {
            seed_session(ctx, "w_eng", "eng");
            let rig = Rig::new(Vec::new());
            // The uniform 404 — no seat there (or this session is itself gone). The browser shows
            // what is actually true: an invitation, a create, or the honest miss.
            *rig.lane_answer.borrow_mut() = Some(Err(ClientError::TargetNotFound {
                target: "workspace".to_owned(),
            }));
            rig.with(|connectors| {
                let out = login(ctx, connectors, Some("topos.example.com/ops"), false).unwrap();
                assert!(out.pending.is_some());
                assert_eq!(
                    rig.enroll.starts.borrow().as_slice(),
                    &[(Some("ops".to_owned()), false)],
                    "the preselection still rides the browser flow"
                );
            });
            assert_eq!(
                sessions::read_sessions(ctx.fs, &ctx.layout)
                    .unwrap()
                    .sessions
                    .len(),
                1,
                "a miss mints nothing"
            );
        });
    }

    #[test]
    fn a_lane_fault_is_reported_not_swallowed() {
        let home = scratch("lane-fault");
        with_ctx(&home, |ctx| {
            seed_session(ctx, "w_eng", "eng");
            let rig = Rig::new(Vec::new());
            *rig.lane_answer.borrow_mut() = Some(Err(ClientError::Plane("unreachable".to_owned())));
            rig.with(|connectors| {
                // A transport fault is NOT a miss: falling through to the browser would ask a
                // human to approve something the lane may well have been about to do.
                let err = login(ctx, connectors, Some("topos.example.com/ops"), false).unwrap_err();
                assert_eq!(err.code(), "PLANE_ERROR");
                assert!(rig.enroll.starts.borrow().is_empty(), "no flow was started");
            });
        });
    }

    #[test]
    fn a_pending_session_grant_persists_pending_and_skips_the_count() {
        let home = scratch("pend");
        with_ctx(&home, |ctx| {
            let rig = Rig::new(vec![granted(LinkStatus::Pending)]);
            rig.with(|connectors| {
                login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap();
                let done = login(ctx, connectors, None, false).unwrap();
                assert_eq!(done.session_status, "pending");
                assert!(done.delivered.is_none());
                assert_eq!(
                    *rig.delivery.calls.borrow(),
                    0,
                    "a pending session must not dial delivery"
                );
            });
            let all = sessions::read_sessions(ctx.fs, &ctx.layout).unwrap();
            assert_eq!(all.sessions[0].status, SESSION_PENDING);
        });
    }

    #[test]
    fn a_fresh_start_declares_the_binding_this_invocation_holds() {
        let home = scratch("bind");
        with_ctx(&home, |ctx| {
            // With a listener held, the START must declare the flow loopback-bound — the
            // write-once fact the approval page's zero-typing card resolves on. (Declaring less
            // once shipped a device-bound flow behind a loopback terminal: the page could not
            // pre-arm, the suppressed code was nowhere, and the wait ran out the whole TTL.)
            let rig = Rig::new(Vec::new());
            rig.with(|connectors| {
                let start = login(ctx, connectors, Some("topos.example.com/eng"), true).unwrap();
                assert!(start.pending.is_some());
                assert_eq!(
                    rig.enroll.starts.borrow().as_slice(),
                    &[(Some("eng".to_owned()), true)]
                );
                let wal = enroll::read_wal(ctx.fs, &ctx.layout).unwrap().unwrap();
                assert!(
                    wal.loopback,
                    "the WAL records the binding the start declared"
                );
                // Without a listener, the same start is the classic typed-code grant.
                enroll::delete_wal(ctx.fs, &ctx.layout).unwrap();
                login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap();
                assert_eq!(
                    rig.enroll.starts.borrow().as_slice(),
                    &[
                        (Some("eng".to_owned()), true),
                        (Some("eng".to_owned()), false)
                    ]
                );
                let wal = enroll::read_wal(ctx.fs, &ctx.layout).unwrap().unwrap();
                assert!(!wal.loopback);
            });
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
