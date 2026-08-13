//! **The address arm** — the Streamable HTTP conversation one verification holds with a server the
//! bundle says every machine dials.
//!
//! ## The sequence, and why it is this one
//!
//! The CURRENT protocol revision has no handshake: a single POST carrying `tools/list` gets both
//! halves of the question this verb asks — "does it answer as an MCP server, and with how many
//! tools" — in ONE round trip. The revision's own backward-compatibility rule for this transport is
//! to attempt a modern request first and inspect the body of a `400` before falling back, precisely
//! because the modern era uses `400` for its OWN errors. So:
//!
//! 1. POST `tools/list` as a modern request. A result ends it.
//! 2. A recognized modern error means the peer IS modern — renegotiate on the versions it named, or
//!    report. Never fall back from one.
//! 3. Anything else that came back as an unrecognized JSON-RPC error, or a `400` whose body is not
//!    a modern error at all, means the handshake era: `initialize` →
//!    `notifications/initialized` → `tools/list`, with the server's session id captured from the
//!    `initialize` response and resent on both follow-ups, and `MCP-Protocol-Version` naming the
//!    version the server negotiated.
//!
//! Round-trip cost: **1** against a modern server, 4 against a legacy one. The reverse order costs
//! 3 and 4 — symmetric in the worst case, worse on the era the specification calls current.
//!
//! ## What this arm never does
//!
//! **It never sends a GET.** In the handshake era a GET to the MCP endpoint opens a standalone SSE
//! stream that stays open until the server closes it; in the current revision it is a `405`.
//! Neither is an answer, and the first is a hang.
//!
//! **It never follows a redirect.** A bundle's address is what every machine dials, so an address
//! that only works after a hop is an address to fix in the bundle. Following it would hide that.

use std::time::Duration;

use serde_json::Value;

use super::wire::{
    self, Era, LEGACY_REVISION, MAX_BODY_BYTES, METHOD_TOOLS_LIST, MODERN_REVISION, Reply,
    StatusRead, Verdict,
};

/// The deadlines this arm runs under. The first two DELIBERATELY match the plane transports', because
/// [`crate::plane_http::transport_reason`] spells those numbers into the sentence a person reads —
/// a different deadline here would print a true-sounding lie. The body deadline is left unset and
/// the whole call is capped instead, so a trickling body is caught by a ceiling whose message names
/// no number at all.
const CONNECT_TIMEOUT_SECS: u64 = 10;
const RECV_RESPONSE_TIMEOUT_SECS: u64 = 30;
/// The hard ceiling on ONE request — DNS, connect, TLS, headers and body together.
const CALL_TIMEOUT_SECS: u64 = 40;

/// The JSON-RPC ids, one per request in the conversation, so a reply can only ever be matched to the
/// request that asked for it.
const ID_MODERN_TOOLS: u64 = 1;
const ID_LEGACY_INIT: u64 = 2;
const ID_LEGACY_TOOLS: u64 = 3;

/// The `Accept` every POST carries: the revision requires a client to take BOTH answer shapes.
const ACCEPT: &str = "application/json, text/event-stream";

/// The session header of the handshake era. The current revision removed protocol-level sessions
/// entirely — a modern server is told to ignore this header and never mint one — so it is sent
/// only on the legacy follow-ups, and only when the server issued one.
const SESSION_HEADER: &str = "mcp-session-id";
const PROTOCOL_HEADER: &str = "mcp-protocol-version";
const METHOD_HEADER: &str = "mcp-method";

/// **The headers this arm speaks for itself** — the protocol half of the conversation.
///
/// A bundle states the headers its server needs, and they ride every request. Not these: an HTTP
/// header may legally appear twice, and the transport APPENDS rather than replaces, so a document
/// naming `Accept` or `MCP-Protocol-Version` would put BOTH values on the wire and a server reading
/// the first — or the joined value — would answer a different question than the one this verb
/// asked. The bundle's spelling of one is therefore dropped, not merged: what a verification
/// negotiates is the verifier's to say.
const RESERVED_HEADERS: [&str; 5] = [
    "accept",
    "content-type",
    PROTOCOL_HEADER,
    METHOD_HEADER,
    SESSION_HEADER,
];

/// Whether a bundle-declared header name is one of [`RESERVED_HEADERS`]. Header names are
/// case-insensitive on the wire, so the comparison is too — `accept` and `Accept` are one header.
fn is_reserved(name: &str) -> bool {
    RESERVED_HEADERS
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

/// The agent one verification dials with: redirects DISABLED (the redirect response itself is the
/// evidence), statuses returned rather than raised, and the deadlines above.
pub(crate) fn agent() -> ureq::Agent {
    agent_with(
        CONNECT_TIMEOUT_SECS,
        RECV_RESPONSE_TIMEOUT_SECS,
        CALL_TIMEOUT_SECS,
    )
}

/// [`agent`] with the deadlines named — the suite dials a socket that answers nothing and cannot
/// wait half a minute to prove it. A shortened agent only moves WHEN a timeout lands, never which
/// verdict it produces, which is what the suite asserts.
pub(crate) fn agent_with(connect: u64, recv: u64, global: u64) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(connect)))
        .timeout_recv_response(Some(Duration::from_secs(recv)))
        .timeout_global(Some(Duration::from_secs(global)))
        .user_agent(concat!("topos/", env!("CARGO_PKG_VERSION")))
        .build();
    ureq::Agent::new_with_config(config)
}

/// One HTTP answer, as far as classification needs it.
struct Answer {
    status: u16,
    content_type: String,
    session: Option<String>,
    body: String,
}

/// **Verify the server at `url`.** `headers` are the bundle's LITERAL, gate-validated header pairs —
/// they ride every request, and no credential can be among them (the publish gate refuses a
/// secret-shaped value, and the placement parse re-checks it).
pub(crate) fn probe(agent: &ureq::Agent, url: &str, headers: &[(String, String)]) -> Verdict {
    let host = crate::plane_http::host_label(url);
    // 1. The modern attempt: one POST, one answer.
    let request = wire::modern_request(ID_MODERN_TOOLS, METHOD_TOOLS_LIST);
    let answer = match post(
        agent,
        url,
        headers,
        &request,
        &[
            (PROTOCOL_HEADER, MODERN_REVISION.to_owned()),
            (METHOD_HEADER, METHOD_TOOLS_LIST.to_owned()),
        ],
    ) {
        Ok(a) => a,
        Err(reason) => return Verdict::NotReachable { reason },
    };
    let status = answer.status;
    match wire::classify_status(&host, status) {
        StatusRead::Settled(v) => return v,
        StatusRead::ReadBody => {}
    }
    match wire::correlate(&answer.content_type, &answer.body, ID_MODERN_TOOLS) {
        // A `400` whose body is not a recognized modern error is the specification's own signal to
        // fall back — the peer rejected the request shape, not the request. A `200` that could not
        // be read as a reply is a different thing entirely, and is a verdict.
        Err(_) if status == 400 => legacy(agent, url, headers, &host),
        Err(detail) => Verdict::NotMcp { detail },
        Ok(reply) => match wire::era_of(reply) {
            Era::Legacy => legacy(agent, url, headers, &host),
            Era::Modern(Reply::Result(result)) => {
                wire::verdict_of_tools(&Reply::Result(result), MODERN_REVISION)
            }
            // `era_of` only calls an ERROR modern when its code is one the current revision
            // defines, so this arm is exactly the renegotiation case.
            Era::Modern(Reply::Error(e)) => renegotiate(agent, url, headers, &host, &e),
        },
    }
}

/// A modern peer that refused this build's revision, over the ONE renegotiation rule
/// ([`wire::renegotiation`]): the handshake where it offers a handshake-era version, the server's
/// own message where it named nothing to negotiate on, and otherwise the honest answer that names
/// the versions rather than blaming the server.
fn renegotiate(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    host: &str,
    error: &wire::RpcError,
) -> Verdict {
    match wire::renegotiation(error) {
        wire::Renegotiation::Handshake => legacy(agent, url, headers, host),
        wire::Renegotiation::Refused => Verdict::NotMcp {
            detail: error.detail(),
        },
        wire::Renegotiation::Unnegotiable => wire::verdict_unsupported_version(&error.supported),
    }
}

/// **The handshake era**: `initialize` → `notifications/initialized` → `tools/list`.
fn legacy(agent: &ureq::Agent, url: &str, headers: &[(String, String)], host: &str) -> Verdict {
    // 1. `initialize` — no protocol header yet, because the version is what it negotiates.
    let answer = match post(
        agent,
        url,
        headers,
        &wire::legacy_initialize(ID_LEGACY_INIT),
        &[],
    ) {
        Ok(a) => a,
        Err(reason) => return Verdict::NotReachable { reason },
    };
    match wire::classify_status(host, answer.status) {
        StatusRead::Settled(v) => return v,
        StatusRead::ReadBody => {}
    }
    let negotiated = match wire::correlate(&answer.content_type, &answer.body, ID_LEGACY_INIT) {
        Ok(Reply::Result(result)) => negotiated_version(&result),
        Ok(Reply::Error(e)) => return Verdict::NotMcp { detail: e.detail() },
        Err(detail) => return Verdict::NotMcp { detail },
    };
    // The session the server assigned, if it assigned one — resent on BOTH follow-ups.
    let mut extra: Vec<(&str, String)> = vec![(PROTOCOL_HEADER, negotiated.clone())];
    if let Some(session) = answer.session {
        extra.push((SESSION_HEADER, session));
    }

    // 2. `notifications/initialized` — a notification, so the answer is a status and no body.
    let answer = match post(agent, url, headers, &wire::legacy_initialized(), &extra) {
        Ok(a) => a,
        Err(reason) => return Verdict::NotReachable { reason },
    };
    if !matches!(answer.status, 200 | 202 | 204) {
        return match wire::classify_status(host, answer.status) {
            StatusRead::Settled(v) => v,
            // A `400` here refused the handshake's second step; there is nothing further to read.
            StatusRead::ReadBody => Verdict::NotMcp {
                detail: format!(
                    "it refused the handshake's second step (HTTP {})",
                    answer.status
                ),
            },
        };
    }

    // 3. `tools/list`, at last.
    let answer = match post(
        agent,
        url,
        headers,
        &wire::legacy_tools_list(ID_LEGACY_TOOLS),
        &extra,
    ) {
        Ok(a) => a,
        Err(reason) => return Verdict::NotReachable { reason },
    };
    match wire::classify_status(host, answer.status) {
        StatusRead::Settled(v) => return v,
        StatusRead::ReadBody => {}
    }
    match wire::correlate(&answer.content_type, &answer.body, ID_LEGACY_TOOLS) {
        Ok(reply) => wire::verdict_of_tools(&reply, &negotiated),
        Err(detail) => Verdict::NotMcp { detail },
    }
}

/// The revision the `initialize` result named, or the one this client asked for when it named none.
fn negotiated_version(result: &Value) -> String {
    result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(LEGACY_REVISION)
        .to_owned()
}

/// One POST, read bounded. The bundle's own headers go on first, minus the [`RESERVED_HEADERS`]
/// this arm speaks for itself — so exactly ONE of each protocol header reaches the server, and it
/// is the verifier's.
///
/// # Errors
/// The plain phrase [`crate::plane_http::transport_reason`] gives a dial that never produced a
/// status — the ONE place a transport fault becomes words in this codebase.
fn post(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
    extra: &[(&str, String)],
) -> Result<Answer, String> {
    let mut request = agent.post(url);
    for (name, value) in headers.iter().filter(|(name, _)| !is_reserved(name)) {
        request = request.header(name, value);
    }
    request = request
        .header("accept", ACCEPT)
        .header("content-type", "application/json");
    for (name, value) in extra {
        request = request.header(*name, value);
    }
    let payload = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let response = request.send(&payload[..]).map_err(|e| {
        crate::plane_http::transport_reason(&crate::plane_http::host_label(url), &e)
    })?;
    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned)
    };
    let content_type = header("content-type").unwrap_or_default();
    let session = header(SESSION_HEADER).filter(|s| !s.trim().is_empty());
    // Bounded by BOTH the byte cap here and the call deadline on the agent.
    let bytes = response
        .into_body()
        .into_with_config()
        .limit(MAX_BODY_BYTES)
        .read_to_vec()
        .unwrap_or_default();
    Ok(Answer {
        status,
        content_type,
        session,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_negotiated_version_is_the_servers_word_and_falls_back_to_the_one_asked_for() {
        assert_eq!(
            negotiated_version(&serde_json::json!({"protocolVersion":"2025-06-18"})),
            "2025-06-18"
        );
        assert_eq!(
            negotiated_version(&serde_json::json!({"protocolVersion":"  "})),
            LEGACY_REVISION
        );
        assert_eq!(negotiated_version(&serde_json::json!({})), LEGACY_REVISION);
    }
}
