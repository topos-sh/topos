//! **The MCP wire, as far as a verification needs it** — the two protocol eras' message shapes, the
//! bounded readers that recover a JSON-RPC reply from either body form, the id correlation, and the
//! ONE classification table every observation is turned into a verdict by.
//!
//! Nothing here does I/O. The remote arm ([`super::remote`]) and the local arm ([`super::local`])
//! carry the bytes; both hand what came back to the same functions below, so an HTTP body and a
//! stdout line are classified by one rule and cannot disagree.
//!
//! ## The two eras
//!
//! The CURRENT protocol revision ([`MODERN_REVISION`]) has **no handshake**: every request stands
//! alone and declares its version in `params._meta`, and Streamable HTTP carries the same value in
//! the `MCP-Protocol-Version` header. Protocol-level sessions and the standalone GET stream were
//! removed in it. The LEGACY revision ([`LEGACY_REVISION`]) and everything before it open with
//! `initialize` → `notifications/initialized`, may assign a session through `Mcp-Session-Id`, and
//! answer `tools/list` only after that.
//!
//! A dual-era client has to detect which it is talking to. The rule the spec gives — and the rule
//! implemented here — is that **the fallback is never keyed to one error code**: a legacy server
//! answers a modern request with an implementation-defined error, or with nothing at all. So an
//! unrecognized JSON-RPC error is an ERA SIGNAL ([`Era::Legacy`]), while the handful of codes the
//! current revision defines prove the peer is modern and must be answered, not fallen back from.

use serde_json::{Value, json};
use topos_types::results::VerifyState;

/// The protocol revision this build speaks first — the CURRENT one.
pub(crate) const MODERN_REVISION: &str = "2026-07-28";
/// The handshake-era revision this build falls back to, and the last one that defined it.
pub(crate) const LEGACY_REVISION: &str = "2025-11-25";

/// How this client names itself to a server. Display and logging only — the protocol says a server
/// must not change behavior on it, and topos never asks it to.
const CLIENT_NAME: &str = "topos";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `_meta` key prefix the current revision reserves for its own per-request metadata.
const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";

/// The JSON-RPC error codes the CURRENT revision defines. Seeing one of these proves the peer
/// speaks a modern revision — the client answers it (renegotiate, or report) and must NOT fall back
/// to the handshake era.
const ERR_UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
const ERR_HEADER_MISMATCH: i64 = -32020;

/// Whether a JSON-RPC error code is one only a MODERN peer produces.
///
/// `-32601` (`Method not found`) is deliberately ABSENT. The current revision does return it for an
/// RPC it does not implement — but it is also what a handshake-era server most commonly answers a
/// pre-`initialize` request with, so on its own it proves nothing about the era, and treating it as
/// modern evidence would strand every legacy server this verb is supposed to reach.
fn is_modern_error(code: i64) -> bool {
    matches!(code, ERR_UNSUPPORTED_PROTOCOL_VERSION | ERR_HEADER_MISMATCH)
}

// =================================================================================================
// The requests
// =================================================================================================

/// The `_meta` block every modern request carries.
fn modern_meta() -> Value {
    json!({
        META_PROTOCOL_VERSION: MODERN_REVISION,
        META_CLIENT_INFO: { "name": CLIENT_NAME, "version": CLIENT_VERSION },
        META_CLIENT_CAPABILITIES: {},
    })
}

/// A modern request for `method`, carrying no parameters of its own.
pub(crate) fn modern_request(id: u64, method: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": { "_meta": modern_meta() },
    })
}

/// The legacy era's opening request.
pub(crate) fn legacy_initialize(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": LEGACY_REVISION,
            "capabilities": {},
            "clientInfo": { "name": CLIENT_NAME, "version": CLIENT_VERSION },
        },
    })
}

/// The legacy era's post-handshake notification (no id — a server answers it with `202`, or with
/// nothing at all on stdio).
pub(crate) fn legacy_initialized() -> Value {
    json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
}

/// The legacy era's tool listing.
pub(crate) fn legacy_tools_list(id: u64) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {} })
}

/// The method names, in one place so a header and a body can never name different ones.
pub(crate) const METHOD_TOOLS_LIST: &str = "tools/list";
pub(crate) const METHOD_DISCOVER: &str = "server/discover";

// =================================================================================================
// The replies
// =================================================================================================

/// One JSON-RPC error, as far as classification needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
    /// The protocol versions an [`ERR_UNSUPPORTED_PROTOCOL_VERSION`] advertised, when it did.
    pub supported: Vec<String>,
}

impl RpcError {
    /// Whether this error proves the peer speaks a modern revision.
    pub(crate) fn is_modern(&self) -> bool {
        is_modern_error(self.code)
    }

    /// The words a verdict shows for it — the server's own message, trimmed to something a terminal
    /// line can hold, with the code beside it so an unfamiliar message is still identifiable.
    pub(crate) fn detail(&self) -> String {
        let message = clip(self.message.trim(), MAX_DETAIL_CHARS);
        if message.is_empty() {
            format!("it answered with protocol error {}", self.code)
        } else {
            format!("it answered with protocol error {} ({message})", self.code)
        }
    }
}

/// What came back for the request we correlated on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply {
    Result(Value),
    Error(RpcError),
}

/// The longest server-supplied phrase a verdict line will carry. A detail is a diagnostic, not a
/// transcript: past this the sentence stops being one a person reads.
const MAX_DETAIL_CHARS: usize = 200;

/// Clip to `max` CHARACTERS (never bytes — a cut inside a multi-byte char would panic), marking the
/// cut so nobody reads a truncation as the server's whole answer.
pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.replace(['\n', '\r'], " ");
    }
    let head: String = s.chars().take(max).collect();
    format!("{}…", head.replace(['\n', '\r'], " "))
}

/// **Recover the reply to request `id` from a body.**
///
/// `content_type` selects the framing: `text/event-stream` is parsed as SSE and every event's data
/// payload is considered in order; anything else is one JSON document. A body that fails BOTH reads
/// is not an MCP answer, and says so.
///
/// Correlation is strict for a RESULT — a result carrying another id (or no id) is never accepted,
/// which is the whole point of sending an id. An ERROR is accepted at a null id too: the spec's own
/// shape for a failure raised before the request was dispatched (an invalid `Origin`, a header
/// mismatch) is an error response with no id, and an error can never become a "responding" verdict
/// anyway.
///
/// # Errors
/// One plain phrase naming what the body was instead — ready to ride a not-an-MCP-server verdict.
pub(crate) fn correlate(content_type: &str, body: &str, id: u64) -> Result<Reply, String> {
    let payloads = if is_event_stream(content_type) {
        let events = sse_events(body);
        if events.is_empty() {
            return Err("it sent an event stream carrying no message".to_owned());
        }
        events
    } else {
        vec![body.to_owned()]
    };
    let mut saw_json = false;
    for payload in &payloads {
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        saw_json = true;
        let Some(object) = value.as_object() else {
            continue;
        };
        let id_field = object.get("id");
        let matches_id = id_field.and_then(Value::as_u64) == Some(id);
        let id_absent = matches!(id_field, None | Some(Value::Null));
        if let Some(error) = object.get("error") {
            if matches_id || id_absent {
                return Ok(Reply::Error(parse_error(error)));
            }
            continue;
        }
        if let Some(result) = object.get("result")
            && matches_id
        {
            return Ok(Reply::Result(result.clone()));
        }
        // A notification (no id, no result/error) on the stream — the spec allows several before
        // the response. Keep reading.
    }
    if saw_json {
        Err("it answered, but nothing in the answer was the reply to this request".to_owned())
    } else {
        Err("its answer was not JSON".to_owned())
    }
}

/// The `error` member of a JSON-RPC error response, read defensively (every field optional).
fn parse_error(error: &Value) -> RpcError {
    let supported = error
        .get("data")
        .and_then(|d| d.get("supported"))
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    RpcError {
        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        supported,
    }
}

/// Whether a `Content-Type` names an SSE stream (parameters and casing ignored, as HTTP requires).
pub(crate) fn is_event_stream(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .eq_ignore_ascii_case("text/event-stream")
}

/// **The tool count in a `tools/list` result**, or `None` when the result is not shaped like one.
/// The count is what the verdict states, so a result without a `tools` ARRAY is not a tool listing
/// and must not be reported as an empty one.
pub(crate) fn tool_count(result: &Value) -> Option<usize> {
    result.get("tools").and_then(Value::as_array).map(Vec::len)
}

/// Whether a `server/discover` result is one — the current revision's `supportedVersions` list is
/// the field that identifies it, and the versions are what the caller may need to renegotiate on.
pub(crate) fn discover_versions(result: &Value) -> Option<Vec<String>> {
    result
        .get("supportedVersions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
}

// =================================================================================================
// Server-sent events
// =================================================================================================

/// The most bytes a verification will read from one response body. A tool listing is small; past
/// this the peer is streaming something a verification has no use for, and the read stops.
pub(crate) const MAX_BODY_BYTES: u64 = 1024 * 1024;
/// The most SSE events one body may carry before the parse stops looking for a reply. A response
/// stream carries progress notifications and then the response; a stream that has sent this many
/// without one is not going to.
const MAX_SSE_EVENTS: usize = 64;

/// **Parse an SSE body into its events' data payloads, in order.**
///
/// The framing rules that matter here, all of which real servers use:
/// - a line beginning with `:` is a COMMENT (the documented keep-alive) and carries no data;
/// - a field is `name: value` with ONE optional leading space stripped from the value; a line with
///   no colon is a field with an empty value;
/// - `event:` and `id:` and `retry:` are read and ignored — a verification correlates on the
///   JSON-RPC id, never on the transport's;
/// - `data:` lines ACCUMULATE, joined by newlines, so a pretty-printed JSON body arrives whole;
/// - a blank line dispatches the accumulated event. Several events in one body are all returned.
///
/// A body whose last event was not terminated by a blank line still dispatches it: the stream was
/// cut after the payload, and the payload is what was asked for.
pub(crate) fn sse_events(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut data = String::new();
    let mut has_data = false;
    for line in body.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            if has_data {
                out.push(std::mem::take(&mut data));
                has_data = false;
            }
            if out.len() >= MAX_SSE_EVENTS {
                return out;
            }
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        if field == "data" {
            if has_data {
                data.push('\n');
            }
            data.push_str(value);
            has_data = true;
        }
    }
    if has_data && out.len() < MAX_SSE_EVENTS {
        out.push(data);
    }
    out
}

// =================================================================================================
// The verdict
// =================================================================================================

/// **What a verification concluded** — the four answers, each carrying the words it prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// It answered as an MCP server and listed `tools` tools, on the revision it NEGOTIATED — which
    /// on the handshake era is the server's choice, not the one this client asked for, so the
    /// reported version is the one actually spoken.
    Responding { tools: usize, protocol: String },
    /// It refused without a sign-in. A HEALTHY server: the agent app holds the credential.
    SignInRequired,
    /// Nothing answered, or the peer is having trouble right now.
    NotReachable { reason: String },
    /// Something answered, but not as an MCP server.
    NotMcp { detail: String },
}

impl Verdict {
    /// The process exit code — `0` responding · `3` sign-in required · `4` not reachable ·
    /// `5` not an MCP server. Documented in the CLI reference, and the whole point of the verb for
    /// a script.
    pub(crate) const fn exit_code(&self) -> u8 {
        match self {
            Self::Responding { .. } => 0,
            Self::SignInRequired => 3,
            Self::NotReachable { .. } => 4,
            Self::NotMcp { .. } => 5,
        }
    }

    /// The typed state the `--json` payload carries.
    pub(crate) const fn state(&self) -> VerifyState {
        match self {
            Self::Responding { .. } => VerifyState::Responding,
            Self::SignInRequired => VerifyState::SignInRequired,
            Self::NotReachable { .. } => VerifyState::NotReachable,
            Self::NotMcp { .. } => VerifyState::NotAnMcpServer,
        }
    }

    /// The ONE sentence, printed and relayed.
    pub(crate) fn line(&self) -> String {
        match self {
            Self::Responding { tools, .. } => format!("responding ({tools} tools)"),
            Self::SignInRequired => {
                "sign-in required - healthy; your agent app completes sign-in on first use"
                    .to_owned()
            }
            Self::NotReachable { reason } => format!("not reachable: {reason}"),
            Self::NotMcp { detail } => {
                format!("reachable but not answering as an MCP server: {detail}")
            }
        }
    }

    /// The reason/detail half of the line, for the typed payload. `None` where the sentence has no
    /// second half.
    pub(crate) fn detail(&self) -> Option<String> {
        match self {
            Self::Responding { .. } | Self::SignInRequired => None,
            Self::NotReachable { reason } => Some(reason.clone()),
            Self::NotMcp { detail } => Some(detail.clone()),
        }
    }

    /// The protocol revision that answered, when one did.
    pub(crate) fn protocol(&self) -> Option<&str> {
        match self {
            Self::Responding { protocol, .. } => Some(protocol.as_str()),
            _ => None,
        }
    }
}

// =================================================================================================
// THE classification table
// =================================================================================================

/// What an HTTP status alone settles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatusRead {
    /// The status IS the answer — no body can change it.
    Settled(Verdict),
    /// The body decides: read it, correlate, and classify what came back.
    ReadBody,
}

/// **The status half of the table.** `host` is the bare hostname (never a URL — a path carries ids
/// nobody needs to read).
///
/// The order is the one a person would ask it in, and two rows are load-bearing:
///
/// - **401/403 outranks everything, and needs no challenge header.** A server demanding a sign-in
///   is a HEALTHY server, and the body it sends with the refusal (an HTML login page, an OAuth
///   problem document) is not evidence about the protocol. A `WWW-Authenticate` challenge is the
///   textbook shape and most servers send one — but it is not required here, because a bare 401
///   still means "it refuses without credentials", which is neither unreachable nor
///   not-an-MCP-server. Gating on the header would push a real, healthy, auth-gated server into
///   the wrong verdict for a header it merely forgot.
/// - **429 and 5xx are never a protocol verdict.** The peer is up and having trouble; saying "not
///   an MCP server" about a server that is merely overloaded would be a lie a person acts on.
///
/// A redirect is NOT followed: the bundle's address is what every machine dials, so an address that
/// only works after a hop is an address that needs fixing in the bundle, and following it would
/// hide that.
pub(crate) fn classify_status(host: &str, status: u16) -> StatusRead {
    let settled = |v: Verdict| StatusRead::Settled(v);
    match status {
        401 | 403 => settled(Verdict::SignInRequired),
        429 => settled(Verdict::NotReachable {
            reason: format!("{host} is rate-limiting this machine (HTTP 429)"),
        }),
        500..=599 => settled(Verdict::NotReachable {
            reason: format!("{host} had an internal error (HTTP {status})"),
        }),
        300..=399 => settled(Verdict::NotMcp {
            detail: format!("the address redirects somewhere else (HTTP {status})"),
        }),
        // 200 carries the reply; 400 carries the era signal (the current revision uses it for its
        // own errors, so the body has to be read before anything is concluded).
        200 | 400 => StatusRead::ReadBody,
        404 | 405 => settled(Verdict::NotMcp {
            detail: format!("it answered HTTP {status}"),
        }),
        _ => settled(Verdict::NotMcp {
            detail: format!("it answered HTTP {status}"),
        }),
    }
}

/// Which era a reply proves the peer is in — the SECOND half of the table, shared by both
/// transports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Era {
    /// The peer answered as a modern server: this reply is the answer.
    Modern(Reply),
    /// The peer is not modern. Fall back to the handshake era.
    Legacy,
}

/// **Read a modern probe's outcome as an era.** A result is modern by construction (only a modern
/// server answers a modern request); an error is modern only when its code is one the current
/// revision defines — everything else is a legacy server saying, in its own words, that it did not
/// understand, and the fallback must not be keyed to any single code.
pub(crate) fn era_of(reply: Reply) -> Era {
    match reply {
        Reply::Result(_) => Era::Modern(reply),
        Reply::Error(ref e) if e.is_modern() => Era::Modern(reply),
        Reply::Error(_) => Era::Legacy,
    }
}

/// **Turn a settled `tools/list` reply into a verdict**, on the revision that answered.
///
/// A protocol error on the FINAL listing is a not-an-MCP-server verdict, including
/// [`ERR_METHOD_NOT_FOUND`]: a peer that speaks the framing but cannot list tools cannot answer the
/// question this verb asks, and reporting it as reachable-and-fine would be worse than saying so.
pub(crate) fn verdict_of_tools(reply: &Reply, protocol: &str) -> Verdict {
    match reply {
        Reply::Result(result) => match tool_count(result) {
            Some(tools) => Verdict::Responding {
                tools,
                protocol: protocol.to_owned(),
            },
            None => Verdict::NotMcp {
                detail: "its answer was not a tool listing".to_owned(),
            },
        },
        Reply::Error(e) => Verdict::NotMcp { detail: e.detail() },
    }
}

/// The verdict for a modern peer this build cannot negotiate a version with. It IS an MCP server,
/// and the honest thing to say is which versions it offered — a person reading this needs to know
/// their topos is the old half.
pub(crate) fn verdict_unsupported_version(supported: &[String]) -> Verdict {
    let detail = if supported.is_empty() {
        format!("it speaks a protocol version this topos does not ({MODERN_REVISION} was refused)")
    } else {
        format!(
            "it speaks only protocol {}, which this topos does not — update topos",
            supported.join(", ")
        )
    };
    Verdict::NotMcp { detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Method not found` — the code the era rule deliberately does NOT treat as modern evidence.
    const ERR_METHOD_NOT_FOUND: i64 = -32601;

    // ---- SSE framing -------------------------------------------------------------------------

    #[test]
    fn sse_parses_comments_multi_line_data_and_several_events() {
        let body = ": keep-alive\n\
                    event: message\n\
                    id: 7\n\
                    data: {\"a\":\n\
                    data: 1}\n\
                    \n\
                    : another comment\n\
                    data: second\n\
                    \n";
        assert_eq!(sse_events(body), vec!["{\"a\":\n1}", "second"]);
    }

    #[test]
    fn sse_strips_exactly_one_leading_space_and_honors_crlf() {
        // CRLF framing, and a value whose own leading spaces past the first survive.
        let body = "data:  two-spaces\r\n\r\ndata: plain\r\n\r\n";
        assert_eq!(sse_events(body), vec![" two-spaces", "plain"]);
    }

    #[test]
    fn sse_dispatches_a_final_event_with_no_terminating_blank_line() {
        assert_eq!(sse_events("data: tail"), vec!["tail"]);
        // A field with no colon carries an empty value and is not data.
        assert_eq!(sse_events("retry\ndata: x\n\n"), vec!["x"]);
        // Comments alone dispatch nothing.
        assert!(sse_events(": one\n: two\n\n").is_empty());
    }

    #[test]
    fn sse_reading_is_bounded_by_the_event_cap() {
        let body = "data: x\n\n".repeat(MAX_SSE_EVENTS + 20);
        assert_eq!(sse_events(&body).len(), MAX_SSE_EVENTS);
    }

    // ---- id correlation ----------------------------------------------------------------------

    #[test]
    fn a_result_is_accepted_only_at_the_id_it_was_asked_for() {
        let ok = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"a"}]}}"#;
        assert!(matches!(
            correlate("application/json", ok, 2),
            Ok(Reply::Result(_))
        ));
        // The SAME payload at another id is never accepted.
        let err = correlate("application/json", ok, 3).unwrap_err();
        assert_eq!(
            err,
            "it answered, but nothing in the answer was the reply to this request"
        );
        // Nor is a result with no id at all.
        let idless = r#"{"jsonrpc":"2.0","result":{"tools":[]}}"#;
        assert!(correlate("application/json", idless, 2).is_err());
    }

    #[test]
    fn an_sse_body_skips_notifications_and_finds_the_correlated_response() {
        let body = ": ping\n\
                    data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\
                    \n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{\"tools\":[1,2,3]}}\n\
                    \n";
        let reply = correlate("text/event-stream; charset=utf-8", body, 9).unwrap();
        let Reply::Result(result) = reply else {
            panic!("expected a result");
        };
        assert_eq!(tool_count(&result), Some(3));
    }

    #[test]
    fn an_error_is_accepted_at_a_null_id_but_a_result_is_not() {
        let e =
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32020,"message":"Header mismatch"}}"#;
        let Ok(Reply::Error(err)) = correlate("application/json", e, 1) else {
            panic!("a null-id error is the spec's pre-dispatch shape");
        };
        assert_eq!(err.code, ERR_HEADER_MISMATCH);
        assert!(err.is_modern());
    }

    #[test]
    fn a_body_that_is_not_json_says_so_and_an_empty_stream_says_something_else() {
        assert_eq!(
            correlate("text/html", "<html>hi</html>", 1).unwrap_err(),
            "its answer was not JSON"
        );
        assert_eq!(
            correlate("text/event-stream", ": only a comment\n\n", 1).unwrap_err(),
            "it sent an event stream carrying no message"
        );
    }

    // ---- the era table -----------------------------------------------------------------------

    #[test]
    fn only_the_current_revisions_own_error_codes_prove_a_modern_peer() {
        let modern = |code: i64| {
            era_of(Reply::Error(RpcError {
                code,
                message: String::new(),
                supported: Vec::new(),
            }))
        };
        assert!(matches!(
            modern(ERR_UNSUPPORTED_PROTOCOL_VERSION),
            Era::Modern(_)
        ));
        assert!(matches!(modern(ERR_HEADER_MISMATCH), Era::Modern(_)));
        // `Method not found` is what a LEGACY server says to a pre-handshake request — the fallback
        // is deliberately not keyed to it, or to any other single code.
        assert_eq!(modern(ERR_METHOD_NOT_FOUND), Era::Legacy);
        assert_eq!(modern(-32600), Era::Legacy);
        assert_eq!(modern(-32602), Era::Legacy);
        // A result is modern by construction.
        assert!(matches!(era_of(Reply::Result(json!({}))), Era::Modern(_)));
    }

    // ---- the status table --------------------------------------------------------------------

    #[test]
    fn the_status_table_settles_every_row_the_contract_names() {
        let sign_in = StatusRead::Settled(Verdict::SignInRequired);
        // Auth outranks the body, and the status alone carries it.
        assert_eq!(classify_status("h", 401), sign_in);
        assert_eq!(classify_status("h", 403), sign_in);
        // Load/outage is never a protocol verdict.
        for status in [429, 500, 502, 503, 599] {
            let StatusRead::Settled(v) = classify_status("acme.test", status) else {
                panic!("{status} must settle");
            };
            assert_eq!(v.exit_code(), 4, "{status}");
        }
        // Redirects are not followed, and 404/405 are not MCP.
        for status in [301, 302, 307, 308, 404, 405, 418] {
            let StatusRead::Settled(v) = classify_status("acme.test", status) else {
                panic!("{status} must settle");
            };
            assert_eq!(v.exit_code(), 5, "{status}");
        }
        // 200 and 400 both need the body: 400 is where the current revision puts its OWN errors.
        assert_eq!(classify_status("h", 200), StatusRead::ReadBody);
        assert_eq!(classify_status("h", 400), StatusRead::ReadBody);
    }

    // ---- the verdict copy --------------------------------------------------------------------

    #[test]
    fn every_verdict_prints_the_contracted_sentence_and_exit_code() {
        let responding = Verdict::Responding {
            tools: 7,
            protocol: MODERN_REVISION.to_owned(),
        };
        assert_eq!(responding.line(), "responding (7 tools)");
        assert_eq!(responding.exit_code(), 0);
        assert_eq!(responding.detail(), None);
        assert_eq!(responding.protocol(), Some("2026-07-28"));

        assert_eq!(
            Verdict::SignInRequired.line(),
            "sign-in required - healthy; your agent app completes sign-in on first use"
        );
        assert_eq!(Verdict::SignInRequired.exit_code(), 3);

        let unreachable = Verdict::NotReachable {
            reason: "could not resolve host weather.acme.example — check your network".to_owned(),
        };
        assert_eq!(
            unreachable.line(),
            "not reachable: could not resolve host weather.acme.example — check your network"
        );
        assert_eq!(unreachable.exit_code(), 4);

        let not_mcp = Verdict::NotMcp {
            detail: "it answered HTTP 404".to_owned(),
        };
        assert_eq!(
            not_mcp.line(),
            "reachable but not answering as an MCP server: it answered HTTP 404"
        );
        assert_eq!(not_mcp.exit_code(), 5);
        assert_eq!(not_mcp.protocol(), None);
    }

    #[test]
    fn a_tools_reply_becomes_a_count_only_when_it_is_a_listing() {
        let listing = Reply::Result(json!({"tools":[{"name":"a"},{"name":"b"}]}));
        assert_eq!(
            verdict_of_tools(&listing, MODERN_REVISION),
            Verdict::Responding {
                tools: 2,
                protocol: MODERN_REVISION.to_owned()
            }
        );
        // An empty listing is a true answer: the server responds, with nothing to offer.
        let empty = Reply::Result(json!({"tools":[]}));
        assert_eq!(verdict_of_tools(&empty, LEGACY_REVISION).exit_code(), 0);
        // A result that is not a listing at all is not a count of zero.
        let odd = Reply::Result(json!({"ok":true}));
        assert_eq!(
            verdict_of_tools(&odd, MODERN_REVISION),
            Verdict::NotMcp {
                detail: "its answer was not a tool listing".to_owned()
            }
        );
        // A protocol error on the final listing is verdict 5, message and code included.
        let failed = Reply::Error(RpcError {
            code: ERR_METHOD_NOT_FOUND,
            message: "Method not found".to_owned(),
            supported: Vec::new(),
        });
        let v = verdict_of_tools(&failed, MODERN_REVISION);
        assert_eq!(v.exit_code(), 5);
        assert_eq!(
            v.detail().unwrap(),
            "it answered with protocol error -32601 (Method not found)"
        );
    }

    #[test]
    fn an_unnegotiable_modern_peer_names_the_versions_it_does_speak() {
        let v = verdict_unsupported_version(&["2027-01-01".to_owned()]);
        assert_eq!(
            v.detail().unwrap(),
            "it speaks only protocol 2027-01-01, which this topos does not — update topos"
        );
        assert_eq!(v.exit_code(), 5);
        assert!(
            verdict_unsupported_version(&[])
                .detail()
                .unwrap()
                .contains(MODERN_REVISION)
        );
    }

    // ---- the requests ------------------------------------------------------------------------

    #[test]
    fn a_modern_request_declares_its_version_where_the_header_must_match_it() {
        let req = modern_request(1, METHOD_TOOLS_LIST);
        assert_eq!(req["method"], METHOD_TOOLS_LIST);
        assert_eq!(req["id"], 1);
        assert_eq!(
            req["params"]["_meta"][META_PROTOCOL_VERSION],
            MODERN_REVISION
        );
        assert_eq!(
            req["params"]["_meta"][META_CLIENT_INFO]["name"],
            CLIENT_NAME
        );
        assert!(req["params"]["_meta"][META_CLIENT_CAPABILITIES].is_object());
    }

    #[test]
    fn the_legacy_opening_names_the_handshake_revision_and_the_notification_has_no_id() {
        let init = legacy_initialize(1);
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["params"]["protocolVersion"], LEGACY_REVISION);
        let note = legacy_initialized();
        assert_eq!(note["method"], "notifications/initialized");
        assert!(note.get("id").is_none(), "a notification carries no id");
        assert_eq!(legacy_tools_list(2)["id"], 2);
    }

    #[test]
    fn an_over_long_server_message_is_clipped_on_a_character_boundary() {
        let long = "é".repeat(MAX_DETAIL_CHARS + 50);
        let clipped = clip(&long, MAX_DETAIL_CHARS);
        assert_eq!(clipped.chars().count(), MAX_DETAIL_CHARS + 1);
        assert!(clipped.ends_with('…'));
        // Newlines never reach a terminal line.
        assert_eq!(clip("a\nb\r\nc", 100), "a b  c");
    }

    #[test]
    fn a_discover_result_is_recognized_by_its_version_list() {
        let result = json!({"resultType":"complete","supportedVersions":["2026-07-28"]});
        assert_eq!(
            discover_versions(&result),
            Some(vec!["2026-07-28".to_owned()])
        );
        assert_eq!(discover_versions(&json!({"tools":[]})), None);
    }

    #[test]
    fn an_unsupported_version_error_carries_the_list_to_renegotiate_on() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32022,
            "message":"Unsupported protocol version",
            "data":{"supported":["2026-07-28","2025-11-25"],"requested":"1900-01-01"}}}"#;
        let Ok(Reply::Error(e)) = correlate("application/json", body, 1) else {
            panic!("expected an error reply");
        };
        assert_eq!(e.supported, vec!["2026-07-28", "2025-11-25"]);
        assert!(e.is_modern());
    }
}
