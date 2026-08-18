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
//! **Login is the acceptance event.** On this machine's FIRST connection to a workspace the
//! login writes that workspace's feed line (`"<host>/<ws>" = "*"`) into `~/.topos/topos.toml`
//! (creating the file if needed) — automatic, undo-led on the receipt — and from then on
//! delivery is silent, npm-style: no consent layer, no per-bundle first-trust asks for
//! workspace content. First-and-never-again is a machine fact (`state/connected.json`, the
//! engine-internal witness that survives logout): a feed line someone deleted stays deleted,
//! and login does not argue with it. The flow is the RFC-8628 shape: card fetch at the
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
use crate::manifest::document::EntryValue;
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

/// Builds a CREDENTIALED directory transport for `(base_url, credential)` — the receipt's one
/// identity read (`me`), naming who signed in.
pub(crate) type SessionDirectoryConnect<'a> =
    dyn Fn(&str, &str) -> Box<dyn crate::plane::DirectorySource> + 'a;

/// The network seams `login` needs.
pub(crate) struct LoginConnectors<'a> {
    pub enroll: &'a EnrollConnect<'a>,
    pub delivery: &'a SessionDeliveryConnect<'a>,
    /// The lane a machine already logged into this server connects the next workspace over.
    pub lane: &'a SessionLaneConnect<'a>,
    /// The session's own directory lane — the best-effort `me` read the receipt's "as <user>"
    /// rides.
    pub directory: &'a SessionDirectoryConnect<'a>,
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
        let same = named
            .as_ref()
            .is_none_or(|t| t.host == wal.host && t.preselect == wal.preselect);
        if same {
            return resume(ctx, connectors, &wal);
        }
        // `login <B>` while a flow toward A is pending: THE LATEST COMMAND WINS — refusing would
        // strand the person behind a ceremony they have already moved on from. The old flow is
        // SETTLED first, never merely dropped: it may already have minted.
        settle_abandoned(ctx, connectors, &wal)?;
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

    // The constant protocol card at the origin declares the API base (same-security re-root) and,
    // where the server is new enough to say so, its own version — judged against this build's floor
    // BEFORE anything is minted, so an unspeakable server costs a sentence rather than a browser
    // round-trip that ends in a wire error.
    let card = (connectors.enroll)(&target.origin).fetch_card(&target.origin)?;
    crate::compat::ensure_server_supported(&card)?;
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
        user: None,
        feed_row_added: false,
        undo: Vec::new(),
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
        ReceiptExtras {
            user: me_display(connectors, &session, status),
            ..ReceiptExtras::default()
        },
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
    // The lane-side connect COMMITS to this server exactly as a fresh flow does, so the same
    // floor holds at the same moment: the card is read first, and a server below the oldest wire
    // this build speaks refuses BEFORE a session is minted. (Today a below-floor server predates
    // this route and would answer the uniform 404 — but the floor moves, and the day it rises
    // past a server that DOES serve the route, this check is what keeps connect from slipping a
    // newer wire past it.)
    let card = (connectors.enroll)(&target.origin).fetch_card(&target.origin)?;
    crate::compat::ensure_server_supported(&card)?;
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
                "this login attempt expired — start over with `topos login`".into(),
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
/// session that already exists. The poll IS the exchange: it can COMMIT server-side while its
/// answer is lost, and the flow code in the WAL is then the only copy of a minted credential.
///
/// Only a PARSED answer is proof of anything. Granted ⇒ keep that session through the whole
/// granted tail, exactly as if this command had completed it (it shows up in `topos status` and
/// its skills arrive on the next update); the receipt that prints is still the NEW login's,
/// because the latest command is what the person asked for. Pending / denied / expired ⇒ nothing
/// was minted and nothing can be: drop the WAL and carry on.
///
/// An INDETERMINATE poll — any transport or parse fault — proves nothing either way, so the WAL
/// is PRESERVED and the new login refuses. Discarding there is exactly the failure this settle
/// exists to prevent. The refusal is bounded and says so: the flow's own expiry (minutes) sweeps
/// the WAL on any later command, and a finished `topos login` clears it immediately. Deliberately
/// no settlement queue — a wait that cannot be seen is worse than a refusal that can.
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
        Ok(_) => enroll::delete_wal(ctx.fs, &ctx.layout),
        Err(e) => {
            let target = if wal.preselect.is_empty() {
                wal.host.clone()
            } else {
                format!("{}/{}", wal.host, wal.preselect)
            };
            Err(ClientError::Enrollment(format!(
                "a login toward {target} is still pending and could not be settled ({}) — nothing \
                 was discarded. Finish it with `topos login`, or it expires by {} and a fresh \
                 login can start.",
                e.detail(),
                super::connect::fmt_rfc3339_millis(wal.expires_at_millis)
            )))
        }
    }
}

/// The shared tail of a login that just MINTED a session (the browser grant and the lane-side
/// connect alike): arm the auto-update trigger, read the acceptance disclosure, write this
/// workspace's feed line on the machine's first connection, and build the receipt. Every step is
/// best-effort — a login is never rolled back because a follow-up read failed; the receipt says
/// so instead.
fn connected_receipt(
    ctx: &Ctx<'_>,
    connectors: &LoginConnectors<'_>,
    session: &Session,
) -> LoginData {
    // Login is the trigger-arming moment for a receiving install (the acceptance event).
    let currency = Some(ctx.triggers.active().install());
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
    // The feed line: written into the machine's own recipe on this machine's FIRST connection to
    // the workspace (creating the file if needed), and never again — a line someone deleted stays
    // deleted. No other workspace's rows are ever touched.
    let (feed_row_added, manifest_note) =
        record_feed_row(ctx, &session.host, &session.workspace_name);
    let undo = if feed_row_added {
        feed_undo(ctx, &session.host, &session.workspace_name)
    } else {
        Vec::new()
    };
    connected_data(
        session,
        &session.status,
        snapshot.as_ref(),
        currency,
        ReceiptExtras {
            user: me_display(connectors, session, &session.status),
            feed_row_added,
            undo,
            manifest_note,
        },
    )
}

/// What only the receipt carries beyond the session row: the signed-in identity, the
/// feed-line-written fact + its undo, and the honest note when the line could not be recorded.
#[derive(Default)]
struct ReceiptExtras {
    user: Option<String>,
    feed_row_added: bool,
    undo: Vec<String>,
    manifest_note: Option<String>,
}

/// The ONE connected-session payload — the browser grant, the lane-side connect, and the
/// already-connected report all render from this shape (`currency` and the feed-line half of
/// `extras` are things only a fresh mint can have done).
fn connected_data(
    session: &Session,
    status: &str,
    snapshot: Option<&DeliverySnapshot>,
    currency: Option<topos_types::TriggerReport>,
    extras: ReceiptExtras,
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
        manifest_note: extras.manifest_note,
        user: extras.user,
        feed_row_added: extras.feed_row_added,
        undo: extras.undo,
    }
}

/// The signed-in person's display identity, read over the session's own directory lane —
/// best-effort (the receipt goes out without the name rather than fail a login on it), and only
/// for an ACTIVE session.
fn me_display(connectors: &LoginConnectors<'_>, session: &Session, status: &str) -> Option<String> {
    if status != SESSION_ACTIVE {
        return None;
    }
    (connectors.directory)(&session.base_url, &session.credential)
        .me(&session.workspace_id)
        .ok()
        .map(|m| m.principal)
        .filter(|p| !p.is_empty())
}

/// The feed reference a RECEIPT spells: the `@ws` sugar when this machine's ONE connected host
/// resolves it — the same default-host rule every verb's sugar uses — else the full `<host>/<ws>`
/// spelling. (The manifest KEY is always the full spelling; this is display only.)
fn feed_display_ref(ctx: &Ctx<'_>, host: &str, workspace: &str) -> String {
    match super::manifest_edit::manifest_host(ctx) {
        Some(h) if h == host => format!("@{workspace}"),
        _ => format!("{host}/{workspace}"),
    }
}

/// The feed line's paste-ready inverse (argv tokens, `topos`-less).
fn feed_undo(ctx: &Ctx<'_>, host: &str, workspace: &str) -> Vec<String> {
    vec![
        "remove".to_owned(),
        "-g".to_owned(),
        feed_display_ref(ctx, host, workspace),
    ]
}

/// Write THIS workspace's feed line into the machine's own `topos.toml` — on this machine's
/// FIRST connection to the workspace, and never again. The witness (`state/connected.json`)
/// decides "first", and it survives logout: a feed line someone deleted stays deleted through
/// any number of re-logins, because absence of the line on a machine that connected before is a
/// deliberate statement. The file is CREATED when it does not exist (header only — login is the
/// only automatic feed-line author); a line already standing counts as success (recorded, never
/// rewritten); and the witness records the workspace only once the line provably stands, so a
/// failed write retries on the next login. Best-effort: EVERY refusal is DISCLOSED on the receipt,
/// never a failed login. Answers `(written_this_login, disclosure)`.
fn record_feed_row(ctx: &Ctx<'_>, host: &str, workspace: &str) -> (bool, Option<String>) {
    // The witness gate — a held witness is silent (nothing went wrong: this machine connected
    // before). An UNREADABLE one reads as held too (never re-add on a guess), but that standstill
    // outlives this login, so it is disclosed by NAME and effect; the document's content never is.
    // (The by-hand way back is spelled only where a note is actually emitted — the silent arms
    // must not pay for a sessions read.)
    let add_back = || format!("topos add -g {}", feed_display_ref(ctx, host, workspace));
    match crate::connected::first_connection(ctx.fs, &ctx.layout, host, workspace) {
        crate::connected::Witness::First => {}
        crate::connected::Witness::Held => return (false, None),
        crate::connected::Witness::Unreadable => {
            return (
                false,
                Some(format!(
                    "{} could not be read — no feed line is written automatically until that \
                     file is removed; run `{}` to take {workspace}'s whole feed",
                    ctx.layout.connected_path().display(),
                    add_back()
                )),
            );
        }
    }
    let reference = format!("{host}/{workspace}");
    let target = super::manifest_edit::global_target(ctx);
    // The same writer lock every manifest mutation takes — this append is a read-modify-write too.
    // Any lock failure lands here — a concurrent writer holding it, but also a lock that cannot
    // be TAKEN at all (the locks dir unwritable) — so the line names the class, not one cause.
    let Ok(_guard) = super::manifest_edit::lock_manifest(ctx, &target.path) else {
        return (
            false,
            Some(format!(
                "{} could not be locked — the feed line was not written; run `{}` to add it",
                target.path.display(),
                add_back()
            )),
        );
    };
    let mut editor = match super::manifest_edit::open_for_edit(ctx, &target) {
        Ok(opened) => opened.editor,
        Err(e) => {
            return (
                false,
                Some(format!(
                    "{} could not be opened ({}) — it was left untouched; add \
                     `\"{reference}\" = \"*\"` there to take {workspace}'s whole feed",
                    target.path.display(),
                    e.detail()
                )),
            );
        }
    };
    if editor.row(&reference).is_some() {
        // The line already stands (written by hand, or an earlier login whose witness write was
        // lost): success — record the witness, rewrite nothing.
        let _ = crate::connected::record(ctx.fs, &ctx.layout, host, workspace);
        return (false, None);
    }
    if let Err(e) = editor.set_row(&reference, &EntryValue::Star) {
        return (
            false,
            Some(format!(
                "{} was left untouched ({e}) — add `\"{reference}\" = \"*\"` there to take \
                 {workspace}'s whole feed",
                target.path.display()
            )),
        );
    }
    match editor.write(ctx.fs, &target.path) {
        Ok(()) => {
            // The line stands: only now does the witness remember the workspace (a failed write
            // above leaves it unrecorded, so the next login retries).
            let _ = crate::connected::record(ctx.fs, &ctx.layout, host, workspace);
            (true, None)
        }
        Err(e) => (
            false,
            Some(format!(
                "{} could not be written ({}) — it was left untouched",
                target.path.display(),
                e.detail()
            )),
        ),
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
            message: "this machine is not logged in anywhere — nothing to log out of".into(),
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
                        "this machine is not logged into workspace '{ws}' — it is logged into: \
                         {}",
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
        ConnectedSession, DeliverySkill, DeviceAuthStart, DirectorySource, EnrollSource,
        EnrolledGrant, EnrolledWorkspace, GovernanceSource,
    };
    use crate::sidecar::Layout;
    use topos_harness::ClaudeCode;
    use topos_types::requests::{WireMe, WireProtocolCard};

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
            roots: None,
        };
        f(&ctx)
    }

    /// The same rig carrying the REAL Claude Code trigger over a temp harness root, handed to the
    /// closure so the completed-login test can read the config login actually edited. Login is the
    /// arming moment for a receiving install, and an inert stand-in can only ever prove that some
    /// report came back — not that a managed entry landed anywhere.
    ///
    /// The root is INJECTED rather than resolved: the registry's resolver reads
    /// `$CLAUDE_CONFIG_DIR`, and a rig writing through a real config store would then arm the
    /// developer's own `settings.json`.
    fn with_arming_ctx<R>(home: &Path, f: impl FnOnce(&Ctx<'_>, &Path) -> R) -> R {
        let fs = RealFs;
        let ids = RealIds;
        let clock = RealClock;
        let plane = crate::plane::InertPlane;
        let follow = crate::plane::InertFollow;
        let claude_root = scratch("claude");
        let harness = ClaudeCode::new(claude_root.clone());
        let trigger = topos_harness::triggers::claude_code_at(claude_root.clone(), &fs);
        let ctx = Ctx {
            progress: crate::progress::silent(),
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: String::new(),
            layout: Layout::new(&home.join(".topos")),
            harness: &harness,
            triggers: crate::ops::Triggers::active_only(trigger.as_ref()),
            plane: &plane,
            follow: &follow,
            roots: None,
        };
        f(&ctx, &claude_root)
    }

    /// What one `device_auth_start` declared: the preselection the browser chooser receives (or
    /// none), and the write-once loopback binding.
    type StartRecord = (Option<String>, bool);

    /// A fake login transport: the card declares an API base + this build's own version facts (a
    /// real card, so every flow test proves a field-carrying card flows end to end); the poll
    /// answers a scripted sequence (pending → granted); every start's declaration is recorded.
    /// State is Rc-shared so the connector can mint a fresh box per call over ONE script.
    #[derive(Clone, Default)]
    struct FakeEnroll {
        polls: Rc<RefCell<Vec<DeviceAuthPoll>>>,
        /// When set, every poll answers this transport fault instead of the script (an old
        /// server gone unreachable).
        poll_fault: Rc<RefCell<Option<String>>>,
        starts: Rc<RefCell<Vec<StartRecord>>>,
        /// The version the served card DECLARES (`None` = a card predating the declaration).
        server_version: Option<String>,
    }
    impl FakeEnroll {
        fn scripted(polls: Vec<DeviceAuthPoll>) -> Self {
            Self {
                polls: Rc::new(RefCell::new(polls)),
                poll_fault: Rc::default(),
                starts: Rc::default(),
                server_version: Some(crate::compat::CURRENT_VERSION.to_owned()),
            }
        }
    }
    impl EnrollSource for FakeEnroll {
        fn fetch_card(&self, _url: &str) -> Result<WireProtocolCard, ClientError> {
            Ok(WireProtocolCard {
                schema_version: 1,
                card: "topos-protocol-card".to_owned(),
                api_base_url: "https://topos.example.com/api".to_owned(),
                server_version: self.server_version.clone(),
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
                mcp_servers: Vec::new(),
                skills: self
                    .names
                    .iter()
                    .map(|n| DeliverySkill {
                        skill_id: format!("sk_{n}"),
                        name: n.clone(),
                        kind: "skill".to_owned(),
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
            _applied: &[crate::plane::AppliedSkillReport],
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

    /// A fake directory lane answering ONLY `me` — the receipt's identity read. `principal:
    /// None` scripts the fault arm (the receipt goes out nameless, never fails).
    #[derive(Clone)]
    struct FakeMe {
        principal: Option<String>,
        calls: Rc<RefCell<usize>>,
    }
    impl Default for FakeMe {
        fn default() -> Self {
            Self {
                principal: Some("robert".to_owned()),
                calls: Rc::default(),
            }
        }
    }
    impl DirectorySource for FakeMe {
        fn me(&self, workspace_id: &str) -> Result<WireMe, ClientError> {
            *self.calls.borrow_mut() += 1;
            match &self.principal {
                Some(p) => Ok(WireMe {
                    workspace_id: workspace_id.to_owned(),
                    name: "eng".to_owned(),
                    display_name: "Engineering".to_owned(),
                    address: "https://topos.example.com/eng".to_owned(),
                    principal: p.clone(),
                    role: "member".to_owned(),
                    invited_by: None,
                    session_status: Some("active".to_owned()),
                }),
                None => Err(ClientError::Plane("unreachable".to_owned())),
            }
        }
        fn channels_index(
            &self,
            _w: &str,
        ) -> Result<topos_types::requests::WireChannelIndex, ClientError> {
            unreachable!()
        }
        fn skills_index(
            &self,
            _w: &str,
        ) -> Result<topos_types::requests::WireSkillIndex, ClientError> {
            unreachable!()
        }
        fn proposals_index(
            &self,
            _w: &str,
        ) -> Result<topos_types::requests::WireProposalIndex, ClientError> {
            unreachable!()
        }
        fn skill_log(
            &self,
            _w: &str,
            _s: &str,
        ) -> Result<topos_types::requests::WireSkillLog, ClientError> {
            unreachable!()
        }
        fn protect_skill(&self, _w: &str, _s: &str, _l: &str) -> Result<(), ClientError> {
            unreachable!()
        }
        fn protect_channel(&self, _w: &str, _c: &str, _l: &str) -> Result<(), ClientError> {
            unreachable!()
        }
        fn add_mcp_server(
            &self,
            _w: &str,
            _b: topos_types::requests::McpAddRequest,
        ) -> Result<topos_types::requests::McpAddedData, ClientError> {
            unreachable!()
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
            link_status: status,
        })
    }

    /// The whole connector set over one script: the enrollment fake, one delivery answer for
    /// every dial, a lane that panics unless a test scripts it, and the `me` identity read
    /// (answering "robert" unless a test scripts the fault).
    struct Rig {
        enroll: FakeEnroll,
        delivery: FakeDelivery,
        me: FakeMe,
        lane_answer: Rc<RefCell<Option<Result<ConnectedSession, ClientError>>>>,
        lane_calls: Rc<RefCell<Vec<(String, String)>>>,
    }

    impl Rig {
        fn new(polls: Vec<DeviceAuthPoll>) -> Self {
            Self {
                enroll: FakeEnroll::scripted(polls),
                delivery: FakeDelivery::default(),
                me: FakeMe::default(),
                lane_answer: Rc::default(),
                lane_calls: Rc::default(),
            }
        }

        fn delivering(mut self, names: &[&str]) -> Self {
            self.delivery.names = names.iter().map(|n| (*n).to_owned()).collect();
            self
        }

        /// The server's card declares `version` instead of this build's own.
        fn declaring(mut self, version: &str) -> Self {
            self.enroll.server_version = Some(version.to_owned());
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
            let directory = {
                let fake = self.me.clone();
                move |_b: &str, _c: &str| -> Box<dyn DirectorySource> { Box::new(fake.clone()) }
            };
            f(&LoginConnectors {
                enroll: &enroll,
                delivery: &delivery,
                lane: &lane,
                directory: &directory,
                web_origin: "https://topos.sh".to_owned(),
            })
        }
    }

    #[test]
    fn login_starts_pends_resumes_and_persists_the_session() {
        let home = scratch("flow");
        with_arming_ctx(&home, |ctx, claude_root| {
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
                // Login ARMS: the report says the trigger is live, and the harness's own config
                // carries the managed entry that makes it so — the file, not just the receipt.
                let armed = done.currency.expect("login arms the trigger");
                assert_eq!(armed.state, topos_types::TriggerState::Active);
                let settings = std::fs::read_to_string(claude_root.join("settings.json")).unwrap();
                assert!(
                    settings.contains("topos update --quiet --hook claude-code"),
                    "the managed sweep landed in the harness config: {settings}"
                );
                assert!(
                    settings.contains("# topos:currency"),
                    "the entry carries the ownership sentinel: {settings}"
                );
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
    fn a_server_below_this_builds_floor_refuses_before_the_flow_starts() {
        let home = scratch("floor");
        with_ctx(&home, |ctx| {
            // The card declares a release older than the oldest wire this build speaks — refuse at
            // the card, before a browser, a flow code, or a WAL exists to clean up.
            let rig = Rig::new(Vec::new()).declaring("0.1.9");
            rig.with(|connectors| {
                let err = login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap_err();
                match &err {
                    ClientError::ServerTooOld { server_version } => {
                        assert_eq!(server_version, "0.1.9");
                    }
                    other => panic!("expected ServerTooOld, got {other:?}"),
                }
                let message = crate::render::safe_message(&err);
                assert!(message.contains("0.1.9"), "{message}");
                // BOTH remedies are named — one is another person's to run, one is this machine's.
                assert!(message.contains("ask whoever runs the server"), "{message}");
                // …and the machine's own remedy is runnable, pinned to the server's release.
                let hint = crate::render::err_hint_tty("login", &["login".to_owned()], &err)
                    .expect("the dead end offers the pin");
                assert!(
                    hint.contains("topos self-update --version v0.1.9"),
                    "{hint}"
                );
                assert!(
                    rig.enroll.starts.borrow().is_empty(),
                    "nothing was started against an unspeakable server"
                );
                assert!(enroll::read_wal(ctx.fs, &ctx.layout).unwrap().is_none());
            });
        });
    }

    #[test]
    fn a_server_at_or_above_the_floor_and_one_declaring_nothing_both_proceed() {
        for (tag, rig) in [
            // The ordinary case: a card carrying today's truth.
            ("current", Rig::new(Vec::new())),
            // A server newer than this build is not this direction's problem (its own 426 is).
            ("newer", Rig::new(Vec::new()).declaring("9.9.9")),
            // A producer predating the declaration says nothing, and silence is not a claim.
            ("silent", {
                let mut rig = Rig::new(Vec::new());
                rig.enroll.server_version = None;
                rig
            }),
        ] {
            let home = scratch(&format!("floor-ok-{tag}"));
            with_ctx(&home, |ctx| {
                rig.with(|connectors| {
                    let start =
                        login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap();
                    assert!(start.pending.is_some(), "{tag}");
                    assert_eq!(rig.enroll.starts.borrow().len(), 1, "{tag}");
                });
            });
        }
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
        // A PARSED pending / denied / expired is proof: nothing was minted and nothing can be, so
        // the old WAL goes and the new target starts fresh.
        for (tag, script) in [
            ("pending", vec![DeviceAuthPoll::Pending]),
            ("denied", vec![DeviceAuthPoll::Denied]),
            ("expired", vec![DeviceAuthPoll::Expired]),
        ] {
            let home = scratch(&format!("restart-{tag}"));
            with_ctx(&home, |ctx| {
                let rig = Rig::new(script);
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
    fn an_unsettleable_flow_refuses_the_restart_and_keeps_its_wal() {
        let home = scratch("restart-indeterminate");
        with_ctx(&home, |ctx| {
            // The poll IS the exchange: a lost answer proves NOTHING. The mint may have committed,
            // and this WAL's flow code would be the only copy of its credential — so the new login
            // refuses rather than discard it.
            let rig = Rig::new(Vec::new());
            *rig.enroll.poll_fault.borrow_mut() = Some("connection reset".to_owned());
            rig.with(|connectors| {
                login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap();
                let err = login(ctx, connectors, Some("topos.example.com/ops"), false).unwrap_err();
                assert_eq!(err.code(), "LOGIN_FAILED");
                let msg = err.to_string();
                for expected in [
                    "topos.example.com/eng",
                    "could not be settled",
                    "nothing was discarded",
                    "`topos login`",
                    "expires by",
                ] {
                    assert!(msg.contains(expected), "{expected:?} missing from {msg}");
                }
                // The old flow stands, untouched; no new one was started behind the refusal.
                let wal = enroll::read_wal(ctx.fs, &ctx.layout).unwrap().unwrap();
                assert_eq!(wal.preselect, "eng");
                assert_eq!(
                    rig.enroll.starts.borrow().as_slice(),
                    &[(Some("eng".to_owned()), false)]
                );

                // The refusal is BOUNDED by the flow's own expiry: the ordinary start-of-command
                // sweep reaps the WAL, and the next login proceeds — even with that old server
                // still unreachable, because with no WAL there is nothing left to settle.
                enroll::sweep_expired_wal(ctx.fs, &ctx.layout, i64::MAX).unwrap();
                assert!(enroll::read_wal(ctx.fs, &ctx.layout).unwrap().is_none());
                let next = login(ctx, connectors, Some("topos.example.com/ops"), false).unwrap();
                assert!(next.pending.is_some());
                assert_eq!(next.name, "ops");
            });
            assert!(
                sessions::read_sessions(ctx.fs, &ctx.layout)
                    .unwrap()
                    .sessions
                    .is_empty()
            );
        });
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
                // The lane connect is a landing too: the first connection writes ops's line.
                assert!(out.feed_row_added);
                assert_eq!(out.undo, vec!["remove", "-g", "@ops"]);
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
    fn the_lane_side_connect_holds_the_same_floor_as_a_fresh_flow() {
        let home = scratch("lane-floor");
        with_ctx(&home, |ctx| {
            seed_session(ctx, "w_eng", "eng");
            // A live session on the host would let connect mint browser-free — but the card now
            // declares a release below this build's floor, and connect commits to the server
            // exactly as a fresh flow does, so it refuses at the same moment: before dialing.
            let rig = Rig::new(Vec::new()).declaring("0.1.9");
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
                let err = login(ctx, connectors, Some("topos.example.com/ops"), false).unwrap_err();
                assert!(
                    matches!(&err, ClientError::ServerTooOld { server_version } if server_version == "0.1.9"),
                    "expected ServerTooOld, got {err:?}"
                );
                assert!(
                    rig.lane_calls.borrow().is_empty(),
                    "the lane never dialed an unspeakable server"
                );
                assert!(rig.enroll.starts.borrow().is_empty(), "no flow was started");
            });
            assert_eq!(
                sessions::read_sessions(ctx.fs, &ctx.layout)
                    .unwrap()
                    .sessions
                    .len(),
                1,
                "the floor mints nothing"
            );
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

    // ---- The feed line (the manifest demand a first connection writes). ----

    fn global_manifest(ctx: &Ctx<'_>) -> PathBuf {
        ctx.layout.home().join(crate::manifest::MANIFEST_FILE)
    }

    #[test]
    fn the_first_connection_writes_the_feed_line_and_the_witness_remembers() {
        let home = scratch("feedline");
        with_ctx(&home, |ctx| {
            let rig = Rig::new(vec![granted(LinkStatus::Active)]);
            rig.with(|connectors| {
                login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap();
                let done = login(ctx, connectors, None, false).unwrap();
                assert!(done.feed_row_added, "the first connection writes the line");
                assert_eq!(done.undo, vec!["remove", "-g", "@eng"]);
                assert_eq!(done.user.as_deref(), Some("robert"));
                assert!(done.manifest_note.is_none());
                // The file was CREATED (header only) and holds exactly this feed line.
                let text = std::fs::read_to_string(global_manifest(ctx)).unwrap();
                assert!(text.contains("\"topos.example.com/eng\" = \"*\""), "{text}");
                // The witness holds the workspace, keyed host + NAME — never on any receipt.
                assert_eq!(
                    crate::connected::first_connection(
                        ctx.fs,
                        &ctx.layout,
                        "topos.example.com",
                        "eng"
                    ),
                    crate::connected::Witness::Held
                );
                // The receipt: the first-connection copy, byte-exact.
                assert_eq!(
                    crate::render::session_login_tty(&done),
                    "signed in to topos.example.com/eng as robert\n\
                     what eng delivers to you installs on this machine\n\
                     (undo: topos remove -g @eng)"
                );
            });
        });
    }

    #[test]
    fn a_manifest_lock_that_cannot_be_taken_is_disclosed_not_silent() {
        let home = scratch("feed-busy");
        with_ctx(&home, |ctx| {
            // The locks directory occupied by a FILE — the writer lock cannot be acquired at all.
            std::fs::create_dir_all(ctx.layout.home()).unwrap();
            std::fs::write(ctx.layout.locks_dir(), b"not a directory").unwrap();
            let (written, note) = record_feed_row(ctx, "topos.example.com", "eng");
            assert!(!written);
            let note =
                note.expect("every refusal is disclosed — a silent one is a lie of omission");
            assert!(note.contains("topos.toml could not be locked"), "{note}");
            assert!(
                note.contains("run `topos add -g topos.example.com/eng`"),
                "{note}"
            );
            // Nothing was written, and the witness stays unrecorded so the next login retries.
            assert!(!ctx.fs.exists(&global_manifest(ctx)));
            assert_eq!(
                crate::connected::first_connection(ctx.fs, &ctx.layout, "topos.example.com", "eng"),
                crate::connected::Witness::First
            );
        });
    }

    #[test]
    fn an_unreadable_witness_discloses_the_standstill_and_never_its_content() {
        let home = scratch("feed-witness");
        with_ctx(&home, |ctx| {
            // An undecipherable witness: login writes no line (it cannot tell a first connection
            // from a deliberate deletion), and that standstill outlives this login — so it is
            // named, with the effect and the way out.
            std::fs::create_dir_all(ctx.layout.state_dir()).unwrap();
            std::fs::write(
                ctx.layout.connected_path(),
                b"not json {\"workspaces\":[\"topos.example.com/secretteam\"]}",
            )
            .unwrap();
            let (written, note) = record_feed_row(ctx, "topos.example.com", "eng");
            assert!(!written);
            let note = note.expect("the standstill is disclosed");
            assert!(note.contains("connected.json could not be read"), "{note}");
            assert!(
                note.contains("no feed line is written automatically until that file is removed"),
                "{note}"
            );
            assert!(
                note.contains("run `topos add -g topos.example.com/eng`"),
                "{note}"
            );
            // The witness's CONTENT never surfaces — only its path and the effect.
            assert!(!note.contains("secretteam"), "{note}");
            // And nothing was written on a guess.
            assert!(!ctx.fs.exists(&global_manifest(ctx)));
        });
    }

    #[test]
    fn a_relogin_never_readds_a_hand_deleted_line() {
        let home = scratch("no-readd");
        with_ctx(&home, |ctx| {
            let rig = Rig::new(vec![granted(LinkStatus::Active)]);
            rig.with(|connectors| {
                login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap();
                login(ctx, connectors, None, false).unwrap();
                // The person deletes the line — a deliberate act login must not argue with.
                std::fs::write(global_manifest(ctx), "[bundles]\n").unwrap();
                let again = login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap();
                assert!(again.pending.is_none(), "already connected — no ceremony");
                assert!(!again.feed_row_added);
                assert!(again.undo.is_empty());
                assert_eq!(
                    std::fs::read_to_string(global_manifest(ctx)).unwrap(),
                    "[bundles]\n",
                    "login never re-adds"
                );
                // The re-login receipt is the one line, byte-exact.
                assert_eq!(
                    crate::render::session_login_tty(&again),
                    "signed in to topos.example.com/eng as robert"
                );
            });
        });
    }

    #[test]
    fn the_witness_survives_logout_so_a_fresh_login_readds_nothing() {
        let home = scratch("witness-logout");
        with_ctx(&home, |ctx| {
            let rig = Rig::new(vec![
                granted(LinkStatus::Active),
                granted(LinkStatus::Active),
            ]);
            rig.with(|connectors| {
                login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap();
                login(ctx, connectors, None, false).unwrap();
                std::fs::write(global_manifest(ctx), "[bundles]\n").unwrap();
                // End the session entirely. The witness is NOT a session record: it must stay.
                let revoke = |_b: &str, _c: &str| -> Box<dyn GovernanceSource> {
                    Box::new(FakeRevoke {
                        calls: Rc::new(RefCell::new(Vec::new())),
                        fail: false,
                    })
                };
                logout(ctx, &revoke, None, false).unwrap();
                assert!(
                    sessions::read_sessions(ctx.fs, &ctx.layout)
                        .unwrap()
                        .sessions
                        .is_empty()
                );
                // A whole fresh browser flow toward the same workspace…
                login(ctx, connectors, Some("topos.example.com/eng"), false).unwrap();
                let done = login(ctx, connectors, None, false).unwrap();
                assert_eq!(done.session_status, "active");
                // …and the deleted line stays deleted: absence is deliberate.
                assert!(!done.feed_row_added);
                assert_eq!(
                    std::fs::read_to_string(global_manifest(ctx)).unwrap(),
                    "[bundles]\n"
                );
            });
        });
    }

    #[test]
    fn a_renamed_workspace_writes_its_new_line_and_destroys_nothing() {
        // The witness keys on host + NAME, so a workspace renamed server-side (same id, new
        // slug) reads as a first connection under the new name: its line lands, and the old
        // line — now inert — is left alone. Login adds; it never deletes.
        let home = scratch("renamed");
        with_ctx(&home, |ctx| {
            std::fs::create_dir_all(ctx.layout.home()).unwrap();
            std::fs::write(
                global_manifest(ctx),
                "[bundles]\n\"topos.example.com/eng\" = \"*\"\n",
            )
            .unwrap();
            crate::connected::record(ctx.fs, &ctx.layout, "topos.example.com", "eng").unwrap();
            let rig = Rig::new(vec![DeviceAuthPoll::Granted(EnrolledGrant {
                credential: "sess-secret".to_owned(),
                session_id: "sn_1".to_owned(),
                workspace: EnrolledWorkspace {
                    // The SAME workspace id…
                    workspace_id: "w_eng".to_owned(),
                    // …under its new name.
                    name: "engineering".to_owned(),
                    display_name: "Engineering".to_owned(),
                },
                link_status: LinkStatus::Active,
            })]);
            rig.with(|connectors| {
                login(
                    ctx,
                    connectors,
                    Some("topos.example.com/engineering"),
                    false,
                )
                .unwrap();
                let done = login(ctx, connectors, None, false).unwrap();
                assert!(done.feed_row_added, "the new name is a first connection");
                let text = std::fs::read_to_string(global_manifest(ctx)).unwrap();
                assert!(
                    text.contains("\"topos.example.com/engineering\" = \"*\""),
                    "{text}"
                );
                assert!(
                    text.contains("\"topos.example.com/eng\" = \"*\""),
                    "the old line is not this login's to delete: {text}"
                );
            });
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
