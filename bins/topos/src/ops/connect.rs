//! The shared connector types + cross-verb network helpers: the transport-builder closures the
//! composition root supplies, the API-base re-root, the machine display name, and the
//! session-based resolver universe.

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::plane::{DirectorySource, EnrollSource, ReconcileTransport};
use crate::resolve;
use crate::sessions::{self, SESSION_ENDED};

use super::reconcile::SessionConnect;

/// Builds the creds-free enrollment transport for a plane base URL (the login flow's card fetch +
/// device-authorization routes are unauthenticated — they MINT the credential).
pub(crate) type EnrollConnect<'a> = dyn Fn(&str) -> Box<dyn EnrollSource> + 'a;

/// Builds a credentialed DIRECTORY transport (describe reads + row ops) for a base URL. LEGACY —
/// the session model builds per-session transports through [`SessionConnect`]; this connector
/// remains only for the composed rigs mid-migration.
pub(crate) type DirectoryConnect<'a> = dyn Fn(&str) -> Box<dyn DirectorySource> + 'a;

/// Builds a credentialed RECONCILE transport (delivery + report + the per-skill read lane on one
/// object) for a base URL. LEGACY — see [`DirectoryConnect`].
pub(crate) type DeliveryConnect<'a> = dyn Fn(&str) -> Box<dyn ReconcileTransport> + 'a;

/// Assemble the resolver universe over the LIVE SESSIONS: one [`resolve::WorkspaceNames`] per
/// session (address name, channel names, catalog skills), each read under that session's own
/// credential. A session whose reads answer the uniform not-found (ended / removed) is skipped —
/// its names must not resolve; a transport fault propagates (resolution must not silently
/// narrow).
pub(crate) fn build_universe_sessions(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
) -> Result<Vec<resolve::WorkspaceNames>, ClientError> {
    let all = sessions::read_sessions(ctx.fs, &ctx.layout)?;
    let mut universe = Vec::new();
    for s in &all.sessions {
        if s.status == SESSION_ENDED {
            continue;
        }
        let transports = connect(s);
        match universe_for(&*transports.directory, &s.workspace_id) {
            Ok(names) => universe.push(names),
            Err(ClientError::TargetNotFound { .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(universe)
}

/// The session universe: the resolver names PLUS the per-workspace transports (each under its
/// session's own credential) — what a governance verb resolves against and then writes through.
pub(crate) struct SessionUniverse {
    pub universe: Vec<resolve::WorkspaceNames>,
    lanes: std::collections::HashMap<String, super::reconcile::SessionTransports>,
}

impl SessionUniverse {
    /// The directory lane for a resolved workspace id.
    pub(crate) fn directory_for(&self, workspace_id: &str) -> Option<&dyn DirectorySource> {
        self.lanes.get(workspace_id).map(|t| &*t.directory)
    }

    /// The contribute-write lane for a resolved workspace id.
    pub(crate) fn contribute_for(
        &self,
        workspace_id: &str,
    ) -> Option<&dyn crate::plane::ContributeSource> {
        self.lanes.get(workspace_id).map(|t| &*t.contribute)
    }
}

/// Build the [`SessionUniverse`] (see [`build_universe_sessions`] for the read semantics).
pub(crate) fn session_universe(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
) -> Result<SessionUniverse, ClientError> {
    let all = sessions::read_sessions(ctx.fs, &ctx.layout)?;
    let mut universe = Vec::new();
    let mut lanes = std::collections::HashMap::new();
    for s in &all.sessions {
        if s.status == SESSION_ENDED {
            continue;
        }
        let transports = connect(s);
        match universe_for(&*transports.directory, &s.workspace_id) {
            Ok(names) => {
                universe.push(names);
                lanes.insert(s.workspace_id.clone(), transports);
            }
            Err(ClientError::TargetNotFound { .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(SessionUniverse { universe, lanes })
}

/// One workspace's resolver names from its member-scoped reads.
pub(crate) fn universe_for(
    directory: &dyn DirectorySource,
    workspace_id: &str,
) -> Result<resolve::WorkspaceNames, ClientError> {
    let me = directory.me(workspace_id)?;
    let channels = directory.channels_index(workspace_id)?;
    let skills = directory.skills_index(workspace_id)?;
    Ok(resolve::WorkspaceNames::from_wire(
        workspace_id,
        &me.name,
        &channels,
        &skills,
    ))
}

/// Resolve the API base a login re-roots onto: the card's declared `api_base_url`, normalized —
/// same-security only (an `https` address may never hand the flow to a plain-http base).
///
/// # Errors
/// [`ClientError::Enrollment`] on an empty/downgrading declared base; the URL-shape refusals from
/// [`validate_base_url`].
pub(crate) fn resolve_api_base(link_base: &str, declared: &str) -> Result<String, ClientError> {
    let declared = declared.trim().trim_end_matches('/');
    if declared.is_empty() {
        return Err(ClientError::Enrollment(
            "the protocol card declared no API base URL; upgrade the server".into(),
        ));
    }
    validate_base_url(declared)?;
    if link_base.starts_with("https://") && !declared.starts_with("https://") {
        return Err(ClientError::Enrollment(
            "refusing to connect: the address is https but the card declares a plain-http API base"
                .into(),
        ));
    }
    Ok(declared.to_owned())
}

/// Refuse an API base that is not a well-formed absolute `http(s)://…` URL (the transport's own `Uri`
/// grammar, so anything accepted here builds cleanly downstream). A malformed base would otherwise
/// surface as a transport error whose message echoes the full URI into the diagnostics log.
fn validate_base_url(base: &str) -> Result<(), ClientError> {
    let well_formed = base.parse::<ureq::http::Uri>().is_ok_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https")) && authority_usable(&uri)
    });
    if well_formed {
        Ok(())
    } else {
        Err(ClientError::Enrollment(
            "the declared API base URL is not a valid http(s) URL".into(),
        ))
    }
}

/// The authority half of the base gate: a non-empty host, and a bracketed literal must be a REAL IPv6
/// address. `http::Uri` itself accepts RFC-3986 IPvFuture-shaped brackets (e.g. `[bad]`), which the
/// transport only rejects LATER — with a URI-echoing error, too late for a URL that carries the token.
fn authority_usable(uri: &ureq::http::Uri) -> bool {
    let Some(authority) = uri.authority() else {
        return false;
    };
    let host_port = authority.as_str().rsplit('@').next().unwrap_or("");
    match host_port.strip_prefix('[') {
        Some(rest) => rest
            .split_once(']')
            .is_some_and(|(v6, _port)| v6.parse::<std::net::Ipv6Addr>().is_ok()),
        None => !host_port.is_empty(),
    }
}

/// The human-readable machine name the approval page shows (`topos CLI (<hostname>)`) — a
/// confused-deputy aid, never authority.
pub(crate) fn machine_name() -> String {
    let uname = rustix::system::uname();
    let node = uname.nodename().to_string_lossy();
    let node = node.trim();
    if node.is_empty() {
        "topos CLI".to_owned()
    } else {
        format!("topos CLI ({node})")
    }
}

/// Format epoch-millis as a coarse RFC-3339 UTC string (seconds precision) — the pending-flow
/// expiry disclosure.
pub(crate) fn fmt_rfc3339_millis(millis: i64) -> String {
    let secs = millis.max(0) as u64 / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = crate::render::civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// The device-code CHALLENGE the loopback approval URL carries (hex sha256 of the flow's device
/// code) — the approval card resolves with zero typing while the short code never rides a URL.
pub(crate) fn device_challenge(device_code: &str) -> String {
    topos_core::digest::to_hex(&topos_core::digest::sha256(device_code.as_bytes()))
}
