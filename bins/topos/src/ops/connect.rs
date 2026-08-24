//! The shared connector type + cross-verb network helpers: the enrollment transport-builder the
//! composition root supplies, the API-base re-root, the machine display name, and the
//! session-based resolver universe.

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::plane::{DirectorySource, EnrollSource};
use crate::resolve;
use crate::sessions::{self, SESSION_ENDED};

use super::reconcile::SessionConnect;

/// Builds the creds-free enrollment transport for a plane base URL (the login flow's card fetch +
/// device-authorization routes are unauthenticated — they MINT the credential).
pub(crate) type EnrollConnect<'a> = dyn Fn(&str) -> Box<dyn EnrollSource> + 'a;

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
        match universe_for(&*transports.directory, &s.workspace_id, &s.host) {
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
        // ONE activity line per workspace — the three member reads underneath would otherwise
        // each announce their own "contacting <host>", seven lines for one governance op.
        let _phase = crate::progress::phase(
            ctx.progress,
            &format!("reading {}/{}", s.host, s.workspace_name),
        );
        match universe_for(&*transports.directory, &s.workspace_id, &s.host) {
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
    host: &str,
) -> Result<resolve::WorkspaceNames, ClientError> {
    let me = directory.me(workspace_id)?;
    let channels = directory.channels_index(workspace_id)?;
    let skills = directory.skills_index(workspace_id)?;
    Ok(resolve::WorkspaceNames::from_wire(
        workspace_id,
        host,
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
            "that address answered without naming its API — the server is older than this topos, \
             or it is not a topos server"
                .into(),
        ));
    }
    validate_base_url(declared)?;
    if link_base.starts_with("https://") && !declared.starts_with("https://") {
        return Err(ClientError::Enrollment(
            "refusing to connect — the address is https but the server names a plain-http API"
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

/// The human-readable machine name the approval page shows (`topos CLI · <user>@<hostname>`) — a
/// confused-deputy aid, never authority. It carries the OS user AND the hostname because the
/// label is the ONLY thing telling two sessions apart on the approval page, the bundle page's
/// "On your machines", and Sessions: a hostname alone collapses a laptop and a build box a
/// person is signed in from into one indistinguishable row.
pub(crate) fn machine_name() -> String {
    let uname = rustix::system::uname();
    let node = uname.nodename().to_string_lossy().into_owned();
    compose_machine_name(std::env::var("USER").ok().as_deref(), &node)
}

/// The label's pure half — the OS user (`$USER`) and the hostname, each dropped when it is
/// missing or blank, so an unnameable machine still gets a label rather than a dangling `@`.
fn compose_machine_name(user: Option<&str>, node: &str) -> String {
    let node = node.trim();
    let user = user.map(str::trim).filter(|u| !u.is_empty());
    match (user, node) {
        (Some(user), "") => format!("topos CLI · {user}"),
        (Some(user), node) => format!("topos CLI · {user}@{node}"),
        (None, "") => "topos CLI".to_owned(),
        (None, node) => format!("topos CLI · {node}"),
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

/// The exact INVERSE of [`fmt_rfc3339_millis`]: parse the RFC 3339 UTC spelling this app speaks
/// (`YYYY-MM-DDTHH:MM:SSZ`) back to epoch millis. Anything else answers `None`, and every caller
/// treats that as "no time recorded" rather than guessing one — a pending line shows elapsed
/// time only; a log row stays undated.
pub(crate) fn parse_rfc3339_utc_millis(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() != 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' || bytes[19] != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Howard Hinnant's days-from-civil (the inverse of `render::civil_from_days`).
    let yy = if m <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hh * 3600 + mm * 60 + ss) * 1000)
}

/// The device-code CHALLENGE the loopback approval URL carries (hex sha256 of the flow's device
/// code) — the approval card resolves with zero typing while the short code never rides a URL.
pub(crate) fn device_challenge(device_code: &str) -> String {
    topos_core::digest::to_hex(&topos_core::digest::sha256(device_code.as_bytes()))
}

#[cfg(test)]
mod machine_name_tests {
    use super::compose_machine_name;

    /// Two machines one person is signed in from must be tellable apart on the approval page and
    /// in Sessions, so the label carries the OS user beside the hostname.
    #[test]
    fn label_carries_user_and_host() {
        assert_eq!(
            compose_machine_name(Some("robert"), "MacBookPro"),
            "topos CLI · robert@MacBookPro"
        );
    }

    /// No usable user name (unset, or blank after trimming) — the hostname alone, never a
    /// dangling separator.
    #[test]
    fn no_user_falls_back_to_host_alone() {
        assert_eq!(
            compose_machine_name(None, "build-box"),
            "topos CLI · build-box"
        );
        assert_eq!(
            compose_machine_name(Some("   "), "build-box"),
            "topos CLI · build-box"
        );
    }

    /// Surrounding whitespace on either half never reaches the label.
    #[test]
    fn halves_are_trimmed() {
        assert_eq!(
            compose_machine_name(Some(" robert "), "  MacBookPro\n"),
            "topos CLI · robert@MacBookPro"
        );
    }

    /// Neither half nameable — the bare product label, as before.
    #[test]
    fn nothing_nameable_keeps_the_bare_label() {
        assert_eq!(compose_machine_name(None, "  "), "topos CLI");
        assert_eq!(
            compose_machine_name(Some("robert"), ""),
            "topos CLI · robert"
        );
    }
}
