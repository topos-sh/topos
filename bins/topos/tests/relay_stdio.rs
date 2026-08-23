//! The relay, proven over the real binary: a mock gateway (std `TcpListener`, one thread per
//! connection) receives what a harness would send `topos relay <url>` over stdio, and the test
//! asserts the whole contract — the session credential attached from the store and never printed,
//! the minted `mcp-session-id` echoed, JSON and SSE reply bodies unwrapped to newline-framed
//! stdout, and each of the relay's own refusals in its exact final copy.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_topos")
}

/// A unique scratch `TOPOS_HOME` seeded with one active session.
fn scratch_home(tag: &str, session_id: &str, credential: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("topos-relay-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("identity")).unwrap();
    let sessions = serde_json::json!({
        "schema_version": 1,
        "sessions": [{
            "host": "topos.example",
            "base_url": "https://topos.example/api",
            "workspace_id": "w_1",
            "workspace_name": "eng",
            "display_name": "Eng",
            "session_id": session_id,
            "credential": credential,
            "status": "active",
            "logged_in_at": 0
        }]
    });
    let path = dir.join("identity/sessions.json");
    std::fs::write(&path, sessions.to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(dir.join("identity"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
    }
    dir
}

/// What the mock gateway answers one POST with.
enum Reply {
    Json(&'static str),
    Sse(&'static str),
    Accepted,
    Unauthorized(&'static str),
}

/// One request the mock gateway answered: the method, the headers that matter, and the body.
#[derive(Debug)]
struct Seen {
    method: String,
    auth: Option<String>,
    session: Option<String>,
    body: String,
}

/// One-thread mock gateway, METHOD-AWARE: each POST consumes the next scripted reply; a GET (the
/// relay's standing event stream) is answered 405 without consuming one; a DELETE (the relay's
/// parting act) is answered 204 and recorded. `seen` records POSTs and DELETEs — the requests the
/// tests assert on. The thread ends once the replies are spent and a DELETE has been seen, or
/// when the process does.
struct Gateway {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

fn gateway(replies: Vec<Reply>) -> Gateway {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    std::thread::spawn(move || {
        let mut replies = replies.into_iter();
        let mut deleted = false;
        loop {
            if replies.len() == 0 && deleted {
                return;
            }
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let mut auth = None;
            let mut session = None;
            let mut length = 0usize;
            reader.read_line(&mut line).unwrap(); // request line
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                let Some((name, value)) = header.split_once(':') else {
                    continue;
                };
                let value = value.trim().to_owned();
                match name.to_ascii_lowercase().as_str() {
                    "authorization" => auth = Some(value),
                    "mcp-session-id" => session = Some(value),
                    "content-length" => length = value.parse().unwrap_or(0),
                    _ => {}
                }
            }
            let method = line.split_whitespace().next().unwrap_or("").to_owned();
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            if method == "GET" {
                // The relay's standing event stream: answer it and HOLD IT OPEN, as a real
                // gateway does — the regression this guards is a listener that held a lock while
                // reading such a stream and deadlocked every other request. The socket is parked
                // (leaked to the test process's end) without consuming a scripted reply.
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n: open\n\n",
                );
                std::mem::forget(stream);
                continue;
            }
            if method == "DELETE" {
                deleted = true;
                record.lock().unwrap().push(Seen {
                    method,
                    auth,
                    session,
                    body: String::new(),
                });
                let _ = stream.write_all(
                    b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                );
                continue;
            }
            let Some(reply) = replies.next() else {
                let _ = stream.write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                );
                continue;
            };
            record.lock().unwrap().push(Seen {
                method,
                auth,
                session,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
            let response = match reply {
                Reply::Json(payload) => format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: conv-1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                    payload.len()
                ),
                Reply::Sse(events) => format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{events}",
                    events.len()
                ),
                Reply::Accepted => {
                    "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_owned()
                }
                Reply::Unauthorized(payload) => format!(
                    "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                    payload.len()
                ),
            };
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    Gateway { base, seen }
}

/// Spawn `topos relay <url>` with the scratch home, feed it `input` lines, close stdin, and
/// collect stdout lines + stderr whole.
fn run_relay(home: &PathBuf, url: &str, input: &[&str]) -> (Vec<String>, String) {
    let mut child: Child = Command::new(bin())
        .arg("relay")
        .arg(url)
        .env("TOPOS_HOME", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for line in input {
            writeln!(stdin, "{line}").unwrap();
        }
    } // dropped: stdin closes, the relay drains and exits
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "relay exited {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).unwrap();
    (
        stdout.lines().map(ToOwned::to_owned).collect(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;

/// The whole happy path in one conversation, STEP-SEQUENCED so each request observably lands
/// before the next is sent (the relay forwards concurrently, and the mock answers by arrival):
/// a JSON reply, a notification's empty 202, an SSE reply — with the credential on every
/// request, never on stdout, and the minted conversation id echoed once it exists.
#[test]
fn a_conversation_round_trips_with_the_credential_attached_and_never_printed() {
    let home = scratch_home("happy", "sn_test1", "sc-relay-secret");
    let gw = gateway(vec![
        Reply::Json(r#"{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"mock"}}}"#),
        Reply::Accepted,
        Reply::Sse("data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\n\n"),
    ]);
    let url = format!("{}/sn_test1/mcps_x", gw.base);
    let mut child: Child = Command::new(bin())
        .arg("relay")
        .arg(&url)
        .env("TOPOS_HOME", &home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let read_reply = |stdout: &mut BufReader<_>| {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        line.trim_end().to_owned()
    };
    let landed = |count: usize| {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while gw.seen.lock().unwrap().len() < count {
            assert!(std::time::Instant::now() < deadline, "request never landed");
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    writeln!(stdin, "{INITIALIZE}").unwrap();
    let first = read_reply(&mut stdout);
    assert!(first.contains("serverInfo"), "{first}");

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    landed(2); // a 202 writes nothing — observed on the gateway side instead

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#).unwrap();
    let second = read_reply(&mut stdout);
    assert!(
        second.contains("tools"),
        "the SSE payload unwrapped: {second}"
    );

    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    let seen = gw.seen.lock().unwrap();
    let posts: Vec<_> = seen.iter().filter(|s| s.method == "POST").collect();
    assert_eq!(posts.len(), 3, "{seen:?}");
    for request in posts.iter() {
        assert_eq!(request.auth.as_deref(), Some("Bearer sc-relay-secret"));
    }
    // The conversation id minted on the first reply rides every later request.
    assert_eq!(posts[1].session.as_deref(), Some("conv-1"));
    assert_eq!(posts[2].session.as_deref(), Some("conv-1"));
    // …and the clean exit told the gateway the conversation is over.
    assert!(
        seen.iter()
            .any(|s| s.method == "DELETE" && s.session.as_deref() == Some("conv-1")),
        "{seen:?}"
    );
    // The credential reached the wire and nothing else.
    assert!(
        !first.contains("sc-relay-secret")
            && !second.contains("sc-relay-secret")
            && !stderr.contains("sc-relay-secret"),
        "{first} {second} {stderr}"
    );
}

/// A PIPELINED BURST — all three messages written before any reply is read — still lands every
/// later request with the conversation id the first reply minted: the first-message gate holds
/// them until the opener completes. This is exactly the shape the live gateway refused before
/// the gate existed.
#[test]
fn a_pipelined_burst_still_carries_the_minted_conversation_id() {
    let home = scratch_home("burst", "sn_burst", "sc-x");
    let gw = gateway(vec![
        Reply::Json(r#"{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"mock"}}}"#),
        Reply::Accepted,
        Reply::Json(r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#),
    ]);
    let url = format!("{}/sn_burst/mcps_x", gw.base);
    let (stdout, _) = run_relay(
        &home,
        &url,
        &[
            INITIALIZE,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        ],
    );
    assert_eq!(stdout.len(), 2, "{stdout:?}");
    let seen = gw.seen.lock().unwrap();
    let posts: Vec<_> = seen.iter().filter(|s| s.method == "POST").collect();
    assert_eq!(posts.len(), 3, "{seen:?}");
    // The first request went alone; both held messages carry what its reply minted.
    assert_eq!(posts[0].session, None);
    assert_eq!(posts[1].session.as_deref(), Some("conv-1"));
    assert_eq!(posts[2].session.as_deref(), Some("conv-1"));
    // …and the first request was the initialize (the gate holds arrival order).
    assert!(posts[0].body.contains("initialize"), "{seen:?}");
}

/// A gateway refusal (its own JSON-RPC error, id null) is forwarded as the reply with the
/// request's id put on it, so the agent can correlate.
#[test]
fn a_gateway_refusal_is_forwarded_with_the_requests_id() {
    let home = scratch_home("refused", "sn_test2", "sc-x");
    let gw = gateway(vec![Reply::Unauthorized(
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"Unauthorized."}}"#,
    )]);
    let url = format!("{}/sn_test2/mcps_x", gw.base);
    let (stdout, _) = run_relay(&home, &url, &[INITIALIZE]);
    assert_eq!(stdout.len(), 1, "{stdout:?}");
    let reply: serde_json::Value = serde_json::from_str(&stdout[0]).unwrap();
    assert_eq!(reply["id"], 1, "correlated to the request: {reply}");
    assert_eq!(reply["error"]["message"], "Unauthorized.");
}

/// No session at all: every request is answered with the not-signed-in sentence, id-matched —
/// the relay stays alive (signing in mid-conversation would answer the next call).
#[test]
fn a_machine_with_no_session_is_refused_in_the_final_copy() {
    let home = scratch_home("signed-out", "sn_other", "sc-x");
    std::fs::remove_file(home.join("identity/sessions.json")).unwrap();
    let (stdout, _) = run_relay(&home, "https://gw.example/sn_gone/mcps_x", &[INITIALIZE]);
    assert_eq!(stdout.len(), 1, "{stdout:?}");
    let reply: serde_json::Value = serde_json::from_str(&stdout[0]).unwrap();
    assert_eq!(reply["id"], 1);
    assert_eq!(
        reply["error"]["message"],
        "topos: not signed in on this machine. Run `topos login`."
    );
}

/// A URL naming a session this store does not hold, while OTHER sessions live here: the
/// stale-entry sentence — the sweep's rewrite is the fix, and the copy says so.
#[test]
fn a_stale_entry_is_told_apart_from_a_signed_out_machine() {
    let home = scratch_home("stale", "sn_current", "sc-x");
    let (stdout, _) = run_relay(&home, "https://gw.example/sn_old/mcps_x", &[INITIALIZE]);
    assert_eq!(stdout.len(), 1, "{stdout:?}");
    let reply: serde_json::Value = serde_json::from_str(&stdout[0]).unwrap();
    assert_eq!(
        reply["error"]["message"],
        "topos: this entry belongs to an earlier sign-in. It updates automatically on the next \
         sweep — or run `topos update`."
    );
}

/// Nothing listens at the address: the unreachable sentence names the host.
#[test]
fn an_unreachable_gateway_is_named_in_the_refusal() {
    let home = scratch_home("unreachable", "sn_test3", "sc-x");
    // A bound-then-dropped listener: the port is real and nothing answers on it.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let url = format!("http://127.0.0.1:{port}/sn_test3/mcps_x");
    let (stdout, _) = run_relay(&home, &url, &[INITIALIZE]);
    assert_eq!(stdout.len(), 1, "{stdout:?}");
    let reply: serde_json::Value = serde_json::from_str(&stdout[0]).unwrap();
    assert_eq!(
        reply["error"]["message"],
        format!("topos: can't reach the gateway at 127.0.0.1:{port}.")
    );
}
