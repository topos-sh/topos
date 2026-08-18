//! `topos verify <name>` — the live check, against STUB SERVERS.
//!
//! Nothing here touches a real network or a real MCP server. The address arm dials a `TcpListener`
//! bound to loopback inside the test, which serves canned bytes and RECORDS every request it saw;
//! the program arm spawns a POSIX shell script written into a temp dir, which answers canned JSON
//! on its stdout. Both are the real production code paths end to end — the same `ureq` agent, the
//! same `std::process` spawn, the same classifier.
//!
//! What is under test:
//!
//! - the four verdicts and the four EXIT CODES (`0`/`3`/`4`/`5`), each with its byte-exact line;
//! - the modern-first sequence: ONE POST for a modern server, carrying the protocol header, the
//!   method header, and both accepted content types — and never a GET, at any point;
//! - both answer framings: a single JSON object and an SSE stream with comments, notifications and
//!   multi-line data ahead of the reply;
//! - the fallback to the handshake era, from a `400` with a non-modern body AND from an
//!   unrecognized JSON-RPC error at `200` — with the server's session id CAPTURED and RESENT on
//!   both follow-ups, beside the version it negotiated;
//! - a redirect that is NOT followed (the server sees exactly one request);
//! - the honesty rails: `429`/`5xx` never becomes a protocol verdict, `401` never becomes anything
//!   but healthy, an uncorrelated id is never accepted;
//! - the SIGN-IN WALK behind a refusal: the three things it can conclude (a server that registers
//!   clients on demand, a server that takes only pre-registered ones, and a chain that could not be
//!   read — which claims nothing), each metadata location it tries, and the budget it stays inside.
//!   Its documents are served by the same loopback stub, so no test here leaves the machine;
//! - the minimal spawn environment, proven by a child that reports what it was given;
//! - the whole verb over a real adopted bundle, so the resolution, the kind gate and the payload
//!   are exercised together.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use topos_harness::mcp::EnvValue;
use topos_types::results::{SignInPath, VerifyState};

use crate::ctx::Ctx;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::ops::{self, ProbeVerdict};
use crate::plane::{InertFollow, InertPlane};
use crate::sidecar::Layout;
use crate::test_support::MockHarness;

// =================================================================================================
// The scratch dir
// =================================================================================================

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-vfy-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap_or(dir);
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// =================================================================================================
// The stub HTTP server
// =================================================================================================

/// One request the stub saw, as far as an assertion needs it. `headers` is the joined view (one
/// value per name, the last one seen) and `header_lines` is EVERY line in order — a header may
/// legally appear twice, and "exactly one of these arrived" is a thing this suite asserts.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    /// The request target — the path the walk asked for, which is the whole question in the
    /// discovery tests (WHICH well-known spelling did it try, and in what order).
    target: String,
    headers: BTreeMap<String, String>,
    header_lines: Vec<(String, String)>,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Every value sent under `name`, in wire order.
    fn header_values(&self, name: &str) -> Vec<&str> {
        let name = name.to_ascii_lowercase();
        self.header_lines
            .iter()
            .filter(|(seen, _)| *seen == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// A loopback HTTP server that answers from a per-request-index script and records what it was
/// asked. Every answer closes its connection, so each request is its own accept — which is also
/// what makes "how many requests did it see" a meaningful assertion.
struct Stub {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
    port: u16,
}

impl Stub {
    /// `answer(index, request)` returns the RAW status line + headers + body, minus the
    /// `Connection: close` and `Content-Length` the harness adds.
    fn serve(
        answer: impl Fn(usize, &Seen) -> (u16, Vec<(&'static str, String)>, String) + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_seen, thread_stop) = (Arc::clone(&seen), Arc::clone(&stop));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(mut stream) = stream else { return };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                let index = {
                    let mut held = thread_seen.lock().unwrap();
                    held.push(request.clone());
                    held.len() - 1
                };
                let (status, headers, body) = answer(index, &request);
                let mut head = format!("HTTP/1.1 {status} X\r\nConnection: close\r\n");
                for (name, value) in headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}/mcp"),
            seen,
            stop,
            port,
        }
    }

    /// A server that answers ONE canned response to everything.
    fn always(status: u16, headers: Vec<(&'static str, String)>, body: &str) -> Self {
        let body = body.to_owned();
        Self::serve(move |_, _| (status, headers.clone(), body.clone()))
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the blocked `accept` so the thread can see the flag and end.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
    }
}

/// Read one HTTP request: the head to the blank line, then exactly `Content-Length` body bytes.
fn read_request(stream: &mut TcpStream) -> Option<Seen> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut start = String::new();
    if reader.read_line(&mut start).ok()? == 0 {
        return None;
    }
    let mut start_line = start.split_whitespace();
    let method = start_line.next().unwrap_or_default().to_owned();
    let target = start_line.next().unwrap_or_default().to_owned();
    let mut headers = BTreeMap::new();
    let mut header_lines = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim().to_ascii_lowercase(), value.trim().to_owned());
            headers.insert(name.clone(), value.clone());
            header_lines.push((name, value));
        }
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(Seen {
        method,
        target,
        headers,
        header_lines,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

const JSON: &str = "application/json";
const SSE: &str = "text/event-stream";

fn ct(value: &str) -> Vec<(&'static str, String)> {
    vec![("Content-Type", value.to_owned())]
}

fn dial(stub: &Stub) -> ProbeVerdict {
    ops::probe_remote(&ops::remote_agent(), &stub.url, &[])
}

// =================================================================================================
// The address arm — the modern sequence
// =================================================================================================

/// EXIT 0. A modern server answers the FIRST request, and that request is a POST carrying the
/// version header, the method header, and both accepted content types. One round trip, no GET.
#[test]
fn a_modern_server_answers_one_post_and_the_verdict_is_responding() {
    let stub = Stub::always(
        200,
        ct(JSON),
        r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"forecast"},{"name":"alerts"}]}}"#,
    );
    let verdict = dial(&stub);
    assert_eq!(verdict.line(), "responding (2 tools)");
    assert_eq!(verdict.exit_code(), 0);
    assert_eq!(verdict.protocol(), Some("2026-07-28"));

    let seen = stub.requests();
    assert_eq!(
        seen.len(),
        1,
        "a modern server costs ONE round trip: {seen:?}"
    );
    let request = &seen[0];
    assert_eq!(request.method, "POST", "a GET would hang on an SSE stream");
    assert_eq!(request.header("mcp-protocol-version"), Some("2026-07-28"));
    assert_eq!(request.header("mcp-method"), Some("tools/list"));
    let accept = request.header("accept").unwrap_or_default();
    assert!(accept.contains(JSON) && accept.contains(SSE), "{accept}");
    // The body's own `_meta` states the same version the header does — a modern server rejects a
    // mismatch outright.
    assert!(
        request
            .body
            .contains(r#""io.modelcontextprotocol/protocolVersion":"2026-07-28""#),
        "{}",
        request.body
    );
}

/// EXIT 0 over an SSE answer, with everything a real stream puts ahead of the reply: a keep-alive
/// comment, event/id fields, a notification, and a payload split across two `data:` lines.
#[test]
fn an_sse_framed_answer_is_read_past_its_comments_and_notifications() {
    let body = ": keep-alive\n\
                event: message\n\
                id: 1\n\
                data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\
                \n\
                : another\n\
                event: message\n\
                data: {\"jsonrpc\":\"2.0\",\"id\":1,\n\
                data:  \"result\":{\"tools\":[{\"name\":\"a\"},{\"name\":\"b\"},{\"name\":\"c\"}]}}\n\
                \n";
    let stub = Stub::always(200, ct(SSE), body);
    let verdict = dial(&stub);
    assert_eq!(verdict.line(), "responding (3 tools)");
    assert_eq!(verdict.exit_code(), 0);
}

// =================================================================================================
// The address arm — sign-in required
// =================================================================================================

/// The self-service sentence: today's line, unchanged, and what a server whose chain nobody could
/// read still says.
const SELF_SERVICE_LINE: &str =
    "sign-in required - healthy; your agent app completes sign-in on first use";
/// The other world's sentence, on the same state and the same exit code.
const MANUAL_LINE: &str = "sign-in required - this server accepts only pre-registered clients or \
                           tokens; an agent cannot complete sign-in by itself";

/// A server that refuses the MCP conversation with `401` and then answers an OAuth discovery walk
/// on its own paths — no TLS, no network beyond loopback, and every document served by the code
/// under test's own client.
///
/// `challenge` is a PATH the `WWW-Authenticate` header points `resource_metadata` at (`None` = a
/// bare refusal, which is what leaves the conventional spellings in charge). `chain(base, target)`
/// answers a document for the paths this particular server publishes at and `None` — a `404` —
/// everywhere else, which is what makes "which spelling did it find?" a real question.
fn oauth_stub(
    challenge: Option<&'static str>,
    chain: impl Fn(&str, &str) -> Option<String> + Send + 'static,
) -> Stub {
    let port: Arc<std::sync::OnceLock<u16>> = Arc::new(std::sync::OnceLock::new());
    let stub = {
        let port = Arc::clone(&port);
        Stub::serve(move |_, request| {
            // The listener's port is set before anything dials it, so this only runs after.
            let base = format!("http://127.0.0.1:{}", port.get().expect("a bound port"));
            if request.method == "GET" {
                return match chain(&base, &request.target) {
                    Some(body) => (200, ct(JSON), body),
                    None => (404, ct(JSON), r#"{"error":"not_found"}"#.to_owned()),
                };
            }
            let mut headers = ct(JSON);
            if let Some(path) = challenge {
                headers.push((
                    "WWW-Authenticate",
                    format!(r#"Bearer error="invalid_token", resource_metadata="{base}{path}""#),
                ));
            }
            (401, headers, r#"{"error":"unauthorized"}"#.to_owned())
        })
    };
    port.set(stub.port).expect("the port is set once");
    stub
}

/// The protected-resource document, naming one authorization server.
fn resource_doc(issuer: &str) -> String {
    format!(r#"{{"resource":"{issuer}/mcp","authorization_servers":["{issuer}"]}}"#)
}

/// An authorization server's metadata, with or without the door an agent registers itself through.
/// The client-id-metadata-document flag rides along on the door-less one: it must not rescue it.
fn as_doc(issuer: &str, registers: bool) -> String {
    let registration = if registers {
        format!(r#","registration_endpoint":"{issuer}/register""#)
    } else {
        r#","client_id_metadata_document_supported":true"#.to_owned()
    };
    format!(
        r#"{{"issuer":"{issuer}","authorization_endpoint":"{issuer}/authorize",
            "token_endpoint":"{issuer}/token"{registration}}}"#
    )
}

/// What a host answering EVERY unknown path serves at these ones: valid JSON, and no metadata.
const CATCH_ALL: &str = r#"{"error":"not_found","message":"no route here","status":404}"#;

/// Every path the stub was asked for with a GET, in order — the walk's own itinerary.
fn walked(stub: &Stub) -> Vec<String> {
    stub.requests()
        .into_iter()
        .filter(|r| r.method == "GET")
        .map(|r| r.target)
        .collect()
}

/// EXIT 3, byte-exact. The challenge header is evidence, not a gate: a bare `401` and a `403` say
/// the same true thing, and all three are HEALTHY.
#[test]
fn every_auth_refusal_is_the_healthy_sign_in_verdict() {
    // A challenge that points at a document the server does not actually serve: the pointer is
    // followed, answers nothing, and the verdict says what it has always said.
    let with_challenge = oauth_stub(Some("/oauth/resource"), |_, _| None);
    let v = dial(&with_challenge);
    assert_eq!(v.line(), SELF_SERVICE_LINE);
    assert_eq!(v.exit_code(), 3);
    assert_eq!(v.state(), VerifyState::SignInRequired);
    assert_eq!(walked(&with_challenge), ["/oauth/resource"]);

    // No challenge header, and an HTML body that would otherwise read as "not an MCP server":
    // the status outranks the body.
    let bare = Stub::always(401, ct("text/html"), "<html>nope</html>");
    assert_eq!(dial(&bare).exit_code(), 3);
    let forbidden = Stub::always(403, ct(JSON), r#"{"error":"forbidden"}"#);
    assert_eq!(dial(&forbidden).line(), SELF_SERVICE_LINE);
}

// =================================================================================================
// The address arm — WHICH sign-in
// =================================================================================================

/// **The reassuring sentence, earned.** The server publishes a protected-resource document, that
/// document names an authorization server, and the authorization server registers clients on
/// demand — so an agent app really does complete this sign-in on first use. The conventional
/// spelling with the resource's own path INSERTED is tried first, and it is the one that answers.
#[test]
fn a_server_that_registers_clients_on_demand_keeps_the_first_use_line() {
    let stub = oauth_stub(None, |base, target| match target {
        "/.well-known/oauth-protected-resource/mcp" => Some(resource_doc(base)),
        "/.well-known/oauth-authorization-server" => Some(as_doc(base, true)),
        _ => None,
    });
    let v = dial(&stub);
    assert_eq!(v.line(), SELF_SERVICE_LINE);
    assert_eq!(v.exit_code(), 3);
    assert_eq!(v.state(), VerifyState::SignInRequired);
    assert_eq!(
        walked(&stub),
        [
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-authorization-server",
        ]
    );
}

/// **The honest sentence.** The same chain, complete, with no `registration_endpoint` at the end of
/// it: nothing an agent does by itself gets in. The verdict, the state and the exit code are the
/// ones a healthy server has always earned — what changes is the sentence a person acts on.
#[test]
fn a_server_that_takes_only_pre_registered_clients_says_so() {
    let stub = oauth_stub(None, |base, target| match target {
        "/.well-known/oauth-protected-resource/mcp" => Some(resource_doc(base)),
        "/.well-known/oauth-authorization-server" => Some(as_doc(base, false)),
        _ => None,
    });
    let v = dial(&stub);
    assert_eq!(v.line(), MANUAL_LINE);
    assert_eq!(v.exit_code(), 3);
    assert_eq!(v.state(), VerifyState::SignInRequired);
}

/// **A chain nobody could read is not evidence.** Three ways for it to break — no document at all,
/// a document naming no authorization server, and an authorization server whose metadata is
/// nowhere — and all three print exactly what this verdict printed before the walk existed. The
/// walk stays inside its budget while proving it.
#[test]
fn an_unreadable_chain_keeps_todays_line_and_stays_bounded() {
    // 1. Nothing published at all.
    let silent = oauth_stub(None, |_, _| None);
    assert_eq!(dial(&silent).line(), SELF_SERVICE_LINE);
    assert_eq!(
        walked(&silent),
        [
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-protected-resource",
        ],
        "both conventional spellings, and then it stops"
    );

    // 2. A protected-resource document that names no authorization server: the chain ends there.
    let nameless = oauth_stub(None, |_, target| {
        (target == "/.well-known/oauth-protected-resource/mcp")
            .then(|| r#"{"resource":"https://acme.test/mcp"}"#.to_owned())
    });
    assert_eq!(dial(&nameless).line(), SELF_SERVICE_LINE);

    // 3. The authorization server is named and then answers nothing: four spellings tried, no
    //    answer, no claim — and the whole walk cost no more than its budget.
    let mute_issuer = oauth_stub(None, |base, target| {
        (target == "/.well-known/oauth-protected-resource/mcp")
            .then(|| resource_doc(&format!("{base}/tenant-7")))
    });
    assert_eq!(dial(&mute_issuer).line(), SELF_SERVICE_LINE);
    let itinerary = walked(&mute_issuer);
    assert!(itinerary.len() <= 6, "{itinerary:?}");
    assert_eq!(
        itinerary,
        [
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-authorization-server/tenant-7",
            "/tenant-7/.well-known/oauth-authorization-server",
            "/.well-known/openid-configuration/tenant-7",
            "/tenant-7/.well-known/openid-configuration",
        ]
    );
}

/// **A `200` is not a document.** A host that answers every unknown path with a catch-all body puts
/// valid JSON at the first spellings the walk tries; believing one would produce the definite
/// "registers nobody" verdict out of a page that answered no question at all. The walk passes over
/// anything without the metadata's own minimum shape and keeps going — and where a real document
/// sits at a later spelling, it is the one that answers.
#[test]
fn a_catch_all_json_body_is_passed_over_for_the_real_document() {
    let stub = oauth_stub(None, |base, target| {
        let issuer = format!("{base}/tenant-7");
        match target {
            "/.well-known/oauth-protected-resource/mcp" => Some(resource_doc(&issuer)),
            // The two RFC 8414 spellings answer, and neither is metadata.
            "/.well-known/oauth-authorization-server/tenant-7"
            | "/tenant-7/.well-known/oauth-authorization-server" => Some(CATCH_ALL.to_owned()),
            // The OpenID one is where this server really publishes — and it registers clients.
            "/.well-known/openid-configuration/tenant-7" => Some(as_doc(&issuer, true)),
            _ => None,
        }
    });
    let v = dial(&stub);
    assert_eq!(v.line(), SELF_SERVICE_LINE);
    assert_eq!(v.exit_code(), 3);
    assert_eq!(v.sign_in(), Some(SignInPath::SelfService));
    assert_eq!(
        walked(&stub).len(),
        4,
        "both catch-alls were passed over, not believed: {:?}",
        walked(&stub)
    );
}

/// The same catch-all with NO real document behind it anywhere: nothing was read, so nothing is
/// claimed — today's line, and no `sign_in` field. Silence is the only safe answer here, because
/// the alternative is a pessimistic verdict a person would act on.
#[test]
fn a_catch_all_with_no_real_document_behind_it_concludes_nothing() {
    let stub = oauth_stub(None, |base, target| match target {
        "/.well-known/oauth-protected-resource/mcp" => Some(resource_doc(base)),
        // EVERY authorization-server spelling answers 200 with the same non-document.
        path if path.starts_with("/.well-known/") => Some(CATCH_ALL.to_owned()),
        _ => None,
    });
    let v = dial(&stub);
    assert_eq!(v.line(), SELF_SERVICE_LINE);
    assert_eq!(v.exit_code(), 3);
    assert_eq!(v.state(), VerifyState::SignInRequired);
    assert_eq!(v.sign_in(), None, "nothing was read, so nothing is claimed");
    assert!(walked(&stub).len() <= 6, "{:?}", walked(&stub));
}

/// **All four authorization-server metadata locations are reached** — each proven by a server that
/// publishes at that one spelling and nowhere else. The path-INSERTED forms are the ones a probe
/// that only appends never finds, and the issuer here has a path of its own so the four spellings
/// are four different URLs.
#[test]
fn every_authorization_server_spelling_is_found() {
    for spelling in [
        "/.well-known/oauth-authorization-server/tenant-7",
        "/tenant-7/.well-known/oauth-authorization-server",
        "/.well-known/openid-configuration/tenant-7",
        "/tenant-7/.well-known/openid-configuration",
    ] {
        let stub = oauth_stub(None, move |base, target| {
            let issuer = format!("{base}/tenant-7");
            match target {
                "/.well-known/oauth-protected-resource/mcp" => Some(resource_doc(&issuer)),
                found if found == spelling => Some(as_doc(&issuer, false)),
                _ => None,
            }
        });
        assert_eq!(dial(&stub).line(), MANUAL_LINE, "{spelling}");
        assert!(walked(&stub).contains(&spelling.to_owned()), "{spelling}");
    }
}

/// **Both conventional protected-resource spellings are reached**, and the challenge's own pointer
/// outranks them: a server that publishes its document somewhere else entirely is still read, and
/// the guesses are never made.
#[test]
fn the_protected_resource_document_is_found_wherever_the_server_keeps_it() {
    // The ORIGIN spelling — the second guess, for a server whose document is not under the
    // endpoint's path.
    let at_origin = oauth_stub(None, |base, target| match target {
        "/.well-known/oauth-protected-resource" => Some(resource_doc(base)),
        "/.well-known/oauth-authorization-server" => Some(as_doc(base, true)),
        _ => None,
    });
    assert_eq!(dial(&at_origin).line(), SELF_SERVICE_LINE);
    assert_eq!(
        walked(&at_origin)[..2],
        [
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-protected-resource",
        ]
    );

    // The server's OWN pointer, at a path no convention would ever guess.
    let pointed = oauth_stub(Some("/oauth/resource"), |base, target| match target {
        "/oauth/resource" => Some(resource_doc(base)),
        "/.well-known/oauth-authorization-server" => Some(as_doc(base, false)),
        _ => None,
    });
    assert_eq!(dial(&pointed).line(), MANUAL_LINE);
    assert_eq!(
        walked(&pointed),
        ["/oauth/resource", "/.well-known/oauth-authorization-server",],
        "the pointer is followed, and the conventions are not guessed at"
    );
}

// =================================================================================================
// The address arm — not reachable (4)
// =================================================================================================

/// EXIT 4 for a peer that is UP and having trouble. Never a protocol verdict: a rate-limited or
/// broken server is not "not an MCP server", and a person acting on that would go and edit a
/// perfectly good bundle.
#[test]
fn load_and_outage_statuses_are_not_reachable_never_a_protocol_verdict() {
    for (status, expected) in [
        (429, "is rate-limiting this machine (HTTP 429)"),
        (500, "had an internal error (HTTP 500)"),
        (503, "had an internal error (HTTP 503)"),
    ] {
        let stub = Stub::always(status, ct(JSON), r#"{"error":"busy"}"#);
        let v = dial(&stub);
        assert_eq!(v.exit_code(), 4, "HTTP {status}");
        assert_eq!(v.state(), VerifyState::NotReachable);
        assert!(
            v.line().starts_with("not reachable: 127.0.0.1 "),
            "{}",
            v.line()
        );
        assert!(v.line().ends_with(expected), "{}", v.line());
    }
}

/// EXIT 4 for a dial that never produced a status at all — the transport half of the table, spoken
/// by the one classifier the plane transports share.
#[test]
fn a_refused_connection_is_not_reachable() {
    // Bind, learn the port, then drop the listener: the port is now closed, so the dial is refused
    // immediately. No network, no waiting.
    let port = {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.local_addr().unwrap().port()
    };
    let url = format!("http://127.0.0.1:{port}/mcp");
    let v = ops::probe_remote(&ops::remote_agent(), &url, &[]);
    assert_eq!(v.exit_code(), 4);
    assert!(v.line().starts_with("not reachable: "), "{}", v.line());
}

/// EXIT 4 for a socket that accepts and then says nothing — the hang this verb's deadlines exist
/// for. The agent is shortened so the suite proves the VERDICT without waiting for the real one.
#[test]
fn a_socket_that_accepts_and_answers_nothing_times_out_as_not_reachable() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        // Accept and HOLD — never write a byte.
        let held: Vec<TcpStream> = listener.incoming().filter_map(Result::ok).collect();
        std::mem::forget(held);
    });
    let url = format!("http://127.0.0.1:{port}/mcp");
    let v = ops::probe_remote(&ops::remote_agent_with(2, 1, 3), &url, &[]);
    assert_eq!(v.exit_code(), 4, "{}", v.line());
    assert_eq!(v.state(), VerifyState::NotReachable);
}

// =================================================================================================
// The address arm — not an MCP server (5)
// =================================================================================================

/// EXIT 5 for a redirect, and the server sees EXACTLY ONE request: a bundle's address is what every
/// machine dials, so an address that only works after a hop is one to fix in the bundle.
#[test]
fn a_redirect_is_not_followed_and_is_not_an_mcp_server() {
    let stub = Stub::always(
        302,
        vec![("Location", "https://elsewhere.test/mcp".to_owned())],
        "",
    );
    let v = dial(&stub);
    assert_eq!(
        v.line(),
        "reachable but not answering as an MCP server: the address redirects somewhere else (HTTP 302)"
    );
    assert_eq!(v.exit_code(), 5);
    assert_eq!(stub.requests().len(), 1, "the hop was not taken");
}

/// EXIT 5 for the statuses that mean "nothing is served here", and for bodies that are not a reply.
#[test]
fn the_not_an_mcp_server_rows_all_land_on_five() {
    let missing = Stub::always(404, ct("text/html"), "<h1>Not Found</h1>");
    let v = dial(&missing);
    assert_eq!(
        v.line(),
        "reachable but not answering as an MCP server: it answered HTTP 404"
    );
    assert_eq!(v.exit_code(), 5);
    assert_eq!(v.state(), VerifyState::NotAnMcpServer);

    let wrong_verb = Stub::always(405, ct("text/plain"), "method not allowed");
    assert_eq!(dial(&wrong_verb).exit_code(), 5);

    // A 200 whose body is not JSON at all.
    let prose = Stub::always(200, ct("text/plain"), "hello, I am a web server");
    let v = dial(&prose);
    assert_eq!(
        v.line(),
        "reachable but not answering as an MCP server: its answer was not JSON"
    );
    assert_eq!(v.exit_code(), 5);

    // A 200 carrying JSON that is not a JSON-RPC reply to THIS request.
    let wrong_id = Stub::always(
        200,
        ct(JSON),
        r#"{"jsonrpc":"2.0","id":99,"result":{"tools":[]}}"#,
    );
    let v = dial(&wrong_id);
    assert_eq!(
        v.line(),
        "reachable but not answering as an MCP server: it answered, but nothing in the answer was \
         the reply to this request"
    );
    assert_eq!(v.exit_code(), 5);

    // A 200 result that is not a tool listing is NOT a count of zero.
    let odd = Stub::always(
        200,
        ct(JSON),
        r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
    );
    assert_eq!(
        dial(&odd).line(),
        "reachable but not answering as an MCP server: its answer was not a tool listing"
    );
}

/// EXIT 5 when the peer IS modern but speaks only versions this build does not — and the verdict
/// names them, because the old half of that conversation is topos.
#[test]
fn a_modern_peer_this_build_cannot_negotiate_with_names_the_versions_it_speaks() {
    let stub = Stub::always(
        400,
        ct(JSON),
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32022,"message":"Unsupported protocol version",
            "data":{"supported":["2029-01-01"],"requested":"2026-07-28"}}}"#,
    );
    let v = dial(&stub);
    assert_eq!(
        v.line(),
        "reachable but not answering as an MCP server: it speaks only protocol 2029-01-01, which \
         this topos does not — update topos"
    );
    assert_eq!(v.exit_code(), 5);
    assert_eq!(
        stub.requests().len(),
        1,
        "a modern error is never fallen back from"
    );
}

// =================================================================================================
// The address arm — the handshake era
// =================================================================================================

/// A legacy server, reached the way the specification prescribes: the modern request comes back
/// `400` with a body that is not a modern error, and the client falls back to the handshake — then
/// CARRIES the session the server assigned, and the version it negotiated, on both follow-ups.
#[test]
fn a_four_hundred_with_a_non_modern_body_falls_back_and_carries_the_session() {
    let stub = Stub::serve(|index, request| match index {
        // 1. The modern attempt: rejected, in a legacy server's own words.
        0 => (
            400,
            ct("text/plain"),
            "Bad Request: Server not initialized".to_owned(),
        ),
        // 2. `initialize` — answers, negotiates DOWN, and assigns a session.
        1 => {
            assert_eq!(
                request.header("mcp-session-id"),
                None,
                "nothing to resend yet"
            );
            (
                200,
                vec![
                    ("Content-Type", JSON.to_owned()),
                    ("Mcp-Session-Id", "sess-abc123".to_owned()),
                ],
                r#"{"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-06-18",
                    "capabilities":{},"serverInfo":{"name":"stub","version":"1"}}}"#
                    .to_owned(),
            )
        }
        // 3. The notification: accepted, no body.
        2 => (202, Vec::new(), String::new()),
        // 4. The listing.
        _ => (
            200,
            ct(JSON),
            r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"only"}]}}"#.to_owned(),
        ),
    });
    let v = dial(&stub);
    assert_eq!(v.line(), "responding (1 tool)");
    assert_eq!(v.exit_code(), 0);
    // The version REPORTED is the one the server chose, not the one this client asked for.
    assert_eq!(v.protocol(), Some("2025-06-18"));

    let seen = stub.requests();
    assert_eq!(
        seen.len(),
        5,
        "one rejected modern attempt + the three-step handshake + the session close"
    );
    assert!(
        seen[1].body.contains(r#""method":"initialize""#),
        "{}",
        seen[1].body
    );
    assert!(
        seen[2].body.contains("notifications/initialized"),
        "{}",
        seen[2].body
    );
    for step in &seen[2..4] {
        assert_eq!(
            step.header("mcp-session-id"),
            Some("sess-abc123"),
            "the session the server issued rides every follow-up"
        );
        assert_eq!(
            step.header("mcp-protocol-version"),
            Some("2025-06-18"),
            "and so does the version it negotiated"
        );
        assert_eq!(step.method, "POST");
    }
    // And the way out: the session the server issued is ENDED, naming itself and its version.
    let close = &seen[4];
    assert_eq!(close.method, "DELETE");
    assert_eq!(close.header("mcp-session-id"), Some("sess-abc123"));
    assert_eq!(close.header("mcp-protocol-version"), Some("2025-06-18"));
    assert_eq!(close.body, "", "a close carries no body");
}

/// The close is not a reward for a good verdict: a handshake that got as far as a session and THEN
/// failed has still taken one, and leaves it behind unless it says so. Here the listing comes back
/// `500` — not reachable, never a protocol verdict — and the `DELETE` goes out all the same.
#[test]
fn a_session_is_closed_on_the_way_out_of_a_failure_too() {
    let stub = Stub::serve(|index, _| match index {
        0 => (400, ct("text/plain"), "not initialized".to_owned()),
        1 => (
            200,
            vec![
                ("Content-Type", JSON.to_owned()),
                ("Mcp-Session-Id", "sess-doomed".to_owned()),
            ],
            r#"{"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-11-25"}}"#.to_owned(),
        ),
        2 => (202, Vec::new(), String::new()),
        // The listing never lands: the server is having trouble.
        _ => (500, ct(JSON), r#"{"error":"boom"}"#.to_owned()),
    });
    let v = dial(&stub);
    assert_eq!(v.exit_code(), 4, "{}", v.line());
    assert_eq!(v.state(), VerifyState::NotReachable);

    let seen = stub.requests();
    assert_eq!(seen.len(), 5, "the verdict failed; the close still went");
    assert_eq!(seen[4].method, "DELETE");
    assert_eq!(seen[4].header("mcp-session-id"), Some("sess-doomed"));
}

/// A `DELETE` a server refuses changes NOTHING — the verdict was decided before it was sent, and a
/// server is allowed to say a client may not end its sessions.
#[test]
fn a_refused_close_never_changes_the_verdict() {
    let stub = Stub::serve(|index, _| match index {
        0 => (400, ct("text/plain"), "not initialized".to_owned()),
        1 => (
            200,
            vec![
                ("Content-Type", JSON.to_owned()),
                ("Mcp-Session-Id", "sess-sticky".to_owned()),
            ],
            r#"{"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-11-25"}}"#.to_owned(),
        ),
        2 => (202, Vec::new(), String::new()),
        3 => (
            200,
            ct(JSON),
            r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"only"}]}}"#.to_owned(),
        ),
        // The close: "you may not end this session".
        _ => (405, ct("text/plain"), "method not allowed".to_owned()),
    });
    let v = dial(&stub);
    assert_eq!(v.line(), "responding (1 tool)");
    assert_eq!(v.exit_code(), 0);
    assert_eq!(stub.requests()[4].method, "DELETE");
}

/// The OTHER legacy shape: `200` with a JSON-RPC error whose code the current revision does not
/// define. The fallback is keyed to no single code — this one is `-32600`, not `-32601`.
#[test]
fn an_unrecognized_json_rpc_error_at_two_hundred_is_also_an_era_signal() {
    let stub = Stub::serve(|index, _| {
        match index {
        0 => (
            200,
            ct(JSON),
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Server not initialized"}}"#
                .to_owned(),
        ),
        1 => (
            200,
            ct(JSON),
            r#"{"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-11-25"}}"#.to_owned(),
        ),
        2 => (202, Vec::new(), String::new()),
        _ => (
            200,
            ct(JSON),
            r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"a"},{"name":"b"}]}}"#.to_owned(),
        ),
    }
    });
    let v = dial(&stub);
    assert_eq!(v.line(), "responding (2 tools)");
    assert_eq!(v.protocol(), Some("2025-11-25"));
    // No session header was issued, so none is sent — an absent session is not an empty one.
    let seen = stub.requests();
    assert_eq!(seen[3].header("mcp-session-id"), None);
    // And nothing is closed that was never opened: the four steps, and no `DELETE`.
    assert_eq!(seen.len(), 4);
    assert!(seen.iter().all(|step| step.method == "POST"), "{seen:?}");
}

/// A modern peer that refuses this build's version but offers a handshake-era one is reached
/// THROUGH the handshake — the renegotiation, rather than a verdict.
#[test]
fn an_unsupported_version_naming_a_handshake_era_version_renegotiates_downward() {
    let stub = Stub::serve(|index, _| match index {
        0 => (
            400,
            ct(JSON),
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32022,"message":"Unsupported",
                "data":{"supported":["2025-11-25"],"requested":"2026-07-28"}}}"#
                .to_owned(),
        ),
        1 => (
            200,
            ct(JSON),
            r#"{"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-11-25"}}"#.to_owned(),
        ),
        2 => (202, Vec::new(), String::new()),
        _ => (
            200,
            ct(JSON),
            r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[]}}"#.to_owned(),
        ),
    });
    let v = dial(&stub);
    assert_eq!(v.line(), "responding (0 tools)");
    assert_eq!(v.exit_code(), 0);
}

/// The bundle's own literal headers ride every request in both eras — they are what the server
/// needs to answer at all for some bundles, and nothing here is a credential (the publish gate
/// refuses a secret-shaped value long before this).
#[test]
fn the_bundles_literal_headers_travel_on_every_request() {
    let stub = Stub::serve(|index, _| match index {
        0 => (400, ct("text/plain"), "nope".to_owned()),
        1 => (
            200,
            ct(JSON),
            r#"{"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-11-25"}}"#.to_owned(),
        ),
        2 => (202, Vec::new(), String::new()),
        _ => (
            200,
            ct(JSON),
            r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[]}}"#.to_owned(),
        ),
    });
    let headers = vec![("X-Region".to_owned(), "eu-west-1".to_owned())];
    let v = ops::probe_remote(&ops::remote_agent(), &stub.url, &headers);
    assert_eq!(v.exit_code(), 0);
    for request in stub.requests() {
        assert_eq!(request.header("x-region"), Some("eu-west-1"));
    }
}

/// A bundle's headers may not SHADOW the ones this verb speaks for itself. The transport appends a
/// header rather than replacing one, so a document naming `Accept` or `MCP-Protocol-Version` would
/// put two of each on the wire and a server reading the first — or the joined value — would answer
/// a different question than the one asked. Exactly one of each arrives, and it is the verifier's;
/// everything else the bundle states still travels.
#[test]
fn a_bundle_header_never_shadows_the_verifiers_own() {
    let stub = Stub::always(
        200,
        ct(JSON),
        r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"one"}]}}"#,
    );
    let headers = vec![
        ("accept".to_owned(), "text/html".to_owned()),
        ("MCP-Protocol-Version".to_owned(), "1999-01-01".to_owned()),
        ("Content-Type".to_owned(), "text/plain".to_owned()),
        ("Mcp-Method".to_owned(), "tools/call".to_owned()),
        ("Mcp-Session-Id".to_owned(), "not-this-clients".to_owned()),
        ("X-Region".to_owned(), "eu-west-1".to_owned()),
    ];
    let v = ops::probe_remote(&ops::remote_agent(), &stub.url, &headers);
    assert_eq!(v.line(), "responding (1 tool)");

    let seen = stub.requests();
    let request = &seen[0];
    assert_eq!(
        request.header_values("accept"),
        ["application/json, text/event-stream"],
        "the bundle's `text/html` would refuse both answer framings"
    );
    assert_eq!(
        request.header_values("mcp-protocol-version"),
        ["2026-07-28"],
        "the version is negotiated by the verifier, never by the document"
    );
    assert_eq!(request.header_values("content-type"), ["application/json"]);
    assert_eq!(request.header_values("mcp-method"), ["tools/list"]);
    // A session is the SERVER's to issue; a bundle naming one names nothing this conversation has.
    assert_eq!(request.header_values("mcp-session-id"), Vec::<&str>::new());
    // Everything that is not the protocol's still rides, exactly as stated.
    assert_eq!(request.header_values("x-region"), ["eu-west-1"]);
}

// =================================================================================================
// The program arm — a stub stdio server
// =================================================================================================

/// Write an executable POSIX shell script and return its path.
#[cfg(unix)]
fn script(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path.display().to_string()
}

/// EXIT 0 over stdio, the modern way: the era probe answers, and the listing follows down the same
/// pipe. Both requests are newline-framed JSON on the child's stdin.
#[cfg(unix)]
#[test]
fn a_modern_stdio_server_is_probed_then_listed() {
    let dir = Scratch::new("stdio-modern");
    let program = script(
        &dir.0,
        "modern.sh",
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *server/discover*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{}}}' ;;
    *tools/list*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"a"},{"name":"b"},{"name":"c"},{"name":"d"}]}}' ;;
  esac
done
"#,
    );
    let v = ops::probe_local(&program, &[], &[]);
    assert_eq!(v.line(), "responding (4 tools)");
    assert_eq!(v.exit_code(), 0);
    assert_eq!(v.protocol(), Some("2026-07-28"));
}

/// EXIT 0 over stdio, the handshake way: the probe is answered with an error a legacy server would
/// give, and the client falls back to `initialize` → `notifications/initialized` → `tools/list` on
/// the SAME process.
#[cfg(unix)]
#[test]
fn a_legacy_stdio_server_is_reached_through_the_handshake() {
    let dir = Scratch::new("stdio-legacy");
    let program = script(
        &dir.0,
        "legacy.sh",
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *notifications/initialized*) : ;;
    *server/discover*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}' ;;
    *initialize*)
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"stub","version":"1"}}}' ;;
    *tools/list*)
      printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"tools":[{"name":"only"}]}}' ;;
  esac
done
"#,
    );
    let v = ops::probe_local(&program, &[], &[]);
    assert_eq!(v.line(), "responding (1 tool)");
    assert_eq!(v.exit_code(), 0);
    assert_eq!(v.protocol(), Some("2025-11-25"));
}

/// EXIT 4 for a program that starts and dies — with the tail of its own stderr, which is usually
/// the whole explanation.
#[cfg(unix)]
#[test]
fn a_program_that_exits_immediately_is_not_reachable_and_says_why() {
    let dir = Scratch::new("stdio-dies");
    let program = script(
        &dir.0,
        "dies.sh",
        "#!/bin/sh\necho \"Cannot find module 'weather-server'\" >&2\nexit 1\n",
    );
    let v = ops::probe_local(&program, &[], &[]);
    assert_eq!(v.exit_code(), 4);
    assert_eq!(v.state(), VerifyState::NotReachable);
    assert!(
        v.line()
            .starts_with("not reachable: the program ended without answering (Cannot find module"),
        "{}",
        v.line()
    );
}

/// EXIT 5 for a program that speaks the framing but cannot list tools.
#[cfg(unix)]
#[test]
fn a_program_that_answers_but_cannot_list_tools_is_not_an_mcp_server() {
    let dir = Scratch::new("stdio-notools");
    let program = script(
        &dir.0,
        "notools.sh",
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *server/discover*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"supportedVersions":["2026-07-28"]}}' ;;
    *tools/list*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"no tools here"}}' ;;
  esac
done
"#,
    );
    let v = ops::probe_local(&program, &[], &[]);
    assert_eq!(
        v.line(),
        "reachable but not answering as an MCP server: it answered with protocol error -32601 (no \
         tools here)"
    );
    assert_eq!(v.exit_code(), 5);
}

/// **The environment policy, proven by the child itself.** The script lists one "tool" per fact it
/// can confirm: it got a `PATH`, it got the entry's literal, it got the machine's value for the
/// slot the bundle left open — and it got NONE of the verifier's own environment (`CARGO_*` is set
/// for this test process by cargo, and must not be inherited).
#[cfg(unix)]
#[test]
fn the_child_gets_a_minimal_environment_and_none_of_the_verifiers() {
    let dir = Scratch::new("stdio-env");
    let program = script(
        &dir.0,
        "env.sh",
        r#"#!/bin/sh
IFS= read -r _probe
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"supportedVersions":["2026-07-28"]}}'
IFS= read -r _list
tools=""
[ -n "$PATH" ] && tools="$tools{\"name\":\"path\"},"
[ "$REGION" = "eu-west-1" ] && tools="$tools{\"name\":\"literal\"},"
[ -n "$HOME" ] && tools="$tools{\"name\":\"home\"},"
[ -z "$CARGO_MANIFEST_DIR" ] && tools="$tools{\"name\":\"no-leak\"},"
tools=${tools%,}
printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[$tools]}}"
"#,
    );
    // Cargo sets this for the test process; the child must never see it.
    assert!(
        std::env::var_os("CARGO_MANIFEST_DIR").is_some(),
        "the leak canary must exist in the verifier's own environment"
    );
    let env = vec![(
        "REGION".to_owned(),
        EnvValue::Literal("eu-west-1".to_owned()),
    )];
    let v = ops::probe_local(&program, &[], &env);
    assert_eq!(v.line(), "responding (4 tools)", "{v:?}");
}

/// The pure policy, beside the spawn that proves it: an inherited slot this machine cannot fill is
/// OMITTED, never emptied — a server testing whether a variable is set must see the truth.
#[test]
fn the_pure_environment_policy_omits_what_this_machine_cannot_fill() {
    let host = |name: &str| match name {
        "PATH" => Some("/usr/bin".to_owned()),
        _ => None,
    };
    let entry = [
        ("A".to_owned(), EnvValue::Literal("1".to_owned())),
        ("B".to_owned(), EnvValue::Inherited),
    ];
    let env = ops::verify_spawn_env(&entry, &host, false);
    assert_eq!(
        env,
        vec![
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("A".to_owned(), "1".to_owned()),
        ]
    );
}

/// **The way out kills the TREE, not just the program topos named.** What a package bundle spawns
/// is a runner (`npx`, `uvx`); the real server is the runner's own child, and killing the runner
/// alone leaves that server running — holding the inherited pipes — once per verification. The
/// stub here has exactly that shape: it forks a long `sleep` and records its pid BEFORE it answers
/// a word, so by the time a verdict exists the grandchild is a fact. After the verdict, nothing of
/// it is left.
#[cfg(unix)]
#[test]
fn the_kill_on_the_way_out_reaches_what_the_runner_forked() {
    let dir = Scratch::new("stdio-tree");
    let pidfile = dir.0.join("grandchild.pid");
    let program = script(
        &dir.0,
        "runner.sh",
        &format!(
            r#"#!/bin/sh
sleep 300 &
echo $! > "{pid}"
while IFS= read -r line; do
  case "$line" in
    *server/discover*)
      printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{{}}}}}}' ;;
    *tools/list*)
      printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"only"}}]}}}}' ;;
  esac
done
"#,
            pid = pidfile.display()
        ),
    );
    let v = ops::probe_local(&program, &[], &[]);
    assert_eq!(v.line(), "responding (1 tool)");

    let raw = std::fs::read_to_string(&pidfile).expect("the runner records what it forked");
    let raw: i32 = raw.trim().parse().expect("a pid");
    let pid = rustix::process::Pid::from_raw(raw).expect("a live pid is never 0");
    // The signal lands asynchronously and the reparented grandchild is reaped by init, so the
    // honest question is asked repeatedly for a moment rather than once.
    let mut gone = false;
    for _ in 0..100 {
        if rustix::process::test_kill_process(pid).is_err() {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !gone {
        // Do not leave the suite's own mess behind on the way to the failure.
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
    assert!(
        gone,
        "the server the runner forked ({raw}) outlived the verification"
    );
}

// =================================================================================================
// The verb, end to end
// =================================================================================================

struct Rig {
    home: Scratch,
    work: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: MockHarness,
}

impl Rig {
    fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(&format!("{tag}-home")),
            work: Scratch::new(&format!("{tag}-work")),
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1_700_000_000_000),
            harness: MockHarness::joining(""),
        }
    }
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            progress: crate::progress::silent(),
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: "d_test".into(),
            layout: Layout::new(&self.home.0.join(".topos")),
            harness: &self.harness,
            triggers: crate::ops::Triggers::active_only(&crate::ops::INERT_TRIGGER),
            plane: &InertPlane,
            follow: &InertFollow,
            roots: Some(crate::ctx::AgentRoots {
                home: self.home.0.clone(),
                cwd: Some(self.work.0.clone()),
            }),
        }
    }
    fn write_manifest(&self) {
        let home = self.home.0.join(".topos");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(crate::manifest::MANIFEST_FILE), "[bundles]\n").unwrap();
    }
}

/// A loopback port that is bound long enough to learn its number, then CLOSED — dialing it is
/// refused instantly, locally, and without a name lookup. The whole-verb tests need an address the
/// publish gate accepts (`https`, which no stub here can serve) and that still answers
/// deterministically; a closed loopback port is both.
fn closed_https_url() -> String {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("https://127.0.0.1:{port}/mcp")
}

/// Record a CONNECTED SERVER in this scope's store, exactly as a delivery does: the document, the
/// catalog revision it came from, and the durable kind marker `verify` asks the classifier for.
/// That record IS the document source — a workspace server has no folder here to read one from.
fn record_connected(ctx: &Ctx<'_>, id: &str, name: &str, document: &str) {
    let sid = crate::id::SkillId::parse(id).unwrap();
    crate::mcp_engine::record_server(
        ctx,
        &sid,
        name,
        "mcpr_0123456789abcdef0123456789abcdef",
        document.as_bytes(),
        false,
        false,
    )
    .unwrap();
    crate::bundle_kind::write_kind_marker(ctx, &sid, crate::bundle_kind::BundleKind::Mcp);
}

/// Verify a recorded server BY NAME — the resolution, the kind gate, the document read off the
/// record, the one selection, the dispatch and the whole payload, through the real verb.
///
/// The verdict here is the not-reachable one, because the selection dials `https` and no stub in
/// this suite serves TLS. What the four verdicts themselves look like is settled above, against
/// real sockets and real child processes; what this test is for is everything AROUND the dial.
#[test]
fn the_verb_resolves_a_bundle_by_name_and_reports_the_live_verdict() {
    let url = closed_https_url();
    let rig = Rig::new("verb");
    rig.write_manifest();
    let ctx = rig.ctx();
    record_connected(
        &ctx,
        "s_weather",
        "weather",
        &format!(
            r#"{{"name":"io.github.acme/weather","description":"Conditions for a named place.",
                "version":"1.4.0","remotes":[{{"type":"streamable-http","url":"{url}"}}]}}"#
        ),
    );

    let data = ops::verify(&ctx, "weather", ops::StoreScope::Here).unwrap();
    assert_eq!(data.name, "weather");
    assert_eq!(data.target, "address");
    assert_eq!(data.address.as_deref(), Some(url.as_str()));
    assert_eq!(data.state, VerifyState::NotReachable);
    assert_eq!(data.exit_code, 4);
    assert_eq!(data.tools, None);
    assert!(data.command.is_empty(), "{:?}", data.command);
    // Nothing answered, so nothing is claimed about the protocol.
    assert_eq!(data.protocol_version, None);
    assert!(data.line.starts_with("not reachable: "), "{}", data.line);
    assert_eq!(
        data.detail.as_deref(),
        Some(&data.line["not reachable: ".len()..])
    );
    // The TTY prints exactly the sentence, and nothing else.
    assert_eq!(crate::render::verify_tty(&data), data.line);

    // `-g` resolves the SAME machine-scope record (the adopt above was machine-wide).
    let global = ops::verify(&ctx, "weather", ops::StoreScope::Machine).unwrap();
    assert_eq!(global.address, data.address);
}

/// **A capability gap is a REFUSAL, not a verdict about the server.** A bundle packaged as `oci`
/// was reported `not reachable: packaged as oci, which topos cannot set up yet` on exit 4 — the
/// code a script retries on, over a condition no retry can ever change. Nothing was dialed and
/// nothing ran: it exits 1 with the gap's own code, like every other "this command cannot be run
/// as asked", and the sentence says whose limitation it is.
#[test]
fn a_bundle_this_build_cannot_set_up_refuses_rather_than_reporting_a_verdict() {
    let rig = Rig::new("gap");
    rig.write_manifest();
    let src = rig.work.0.join("imaged");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("server.json"),
        r#"{"name":"io.github.acme/imaged","description":"A container-packaged server.",
            "version":"1.4.0","packages":[{"registryType":"oci",
            "identifier":"ghcr.io/acme/imaged@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "transport":{"type":"stdio"}}]}"#,
    )
    .unwrap();
    let ctx = rig.ctx();
    ops::add_mcp(&ctx, &src.display().to_string(), true, &Default::default()).unwrap();

    let err = ops::verify(&ctx, "imaged", ops::StoreScope::Here).unwrap_err();
    // The GAP's own code travels: one cause, one word, whichever verb met it.
    assert_eq!(err.code(), "MCP_KIND_UNSUPPORTED");
    assert_eq!(
        err.to_string(),
        "this version of topos cannot set this server up on this machine (packaged as oci)"
    );
}

/// A name nothing here tracks refuses — and a bundle that is a SKILL refuses too, saying what
/// `verify` is for rather than reporting a fake verdict over a bundle with nothing to dial.
#[test]
fn verify_refuses_an_unknown_name_and_a_skill() {
    let rig = Rig::new("refuse");
    rig.write_manifest();
    let ctx = rig.ctx();
    let err = ops::verify(&ctx, "nothing-here", ops::StoreScope::Here).unwrap_err();
    assert_eq!(err.code(), "NO_SUCH_SKILL");

    // A real skill, adopted through the ordinary door.
    let src = rig.work.0.join("deploy");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), "---\nname: deploy\n---\nSteps.\n").unwrap();
    let scope = ops::add_scope(&ctx, true).unwrap();
    ops::adopt_path(&ctx, &scope, &src, ops::KindDeclared::No).unwrap();
    let err = ops::verify(&ctx, "deploy", ops::StoreScope::Here).unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    assert!(
        err.to_string()
            .starts_with("'deploy' is a skill, not an MCP server"),
        "{err}"
    );
}
