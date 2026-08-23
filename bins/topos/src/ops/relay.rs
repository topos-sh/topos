//! `topos relay <url>` — the stdio half of a gateway-routed MCP entry.
//!
//! A workspace-delivered server whose document carries the gateway claim is written into a
//! harness config as `topos relay <gateway-url>` ([`crate::mcp_render::select`]): the address
//! stands in the config in the open, and the credential never enters it. The harness spawns this
//! verb as an ordinary stdio MCP server; each JSON-RPC message it writes is POSTed to that one
//! address with the machine's own session credential attached, and the reply — plain JSON or an
//! SSE stream — comes back as newline-framed messages on stdout.
//!
//! **Transport, not protocol.** Nothing here reads a method or a capability: the gateway bridges
//! protocol revisions itself, and this loop only maps framings (stdio lines ⇄ one HTTP POST per
//! message, SSE events unwrapped as they arrive). The one piece of conversation state kept is the
//! `mcp-session-id` the gateway may mint — echoed back on every later request, exactly as a
//! streamable-http client would.
//!
//! **The credential is read LIVE, per message**, from the session store — the one `0600` file it
//! ever lives in ([`crate::sessions`]). Signing out mid-conversation refuses the next call;
//! signing back in answers it; nothing is cached to go stale and nothing has to be rewritten in
//! any config when a session changes. The URL names the session it was rendered for (its `sn_…`
//! path segment), so a config entry left over from an earlier sign-in is told apart from a
//! machine that is not signed in at all, and each earns its own plain-words answer.
//!
//! **Errors are answers, not exits.** A request that cannot be forwarded is answered with an
//! id-matched JSON-RPC error carrying one honest sentence; a notification (no id) has no reply
//! channel and lands one stderr line instead. The process ends when the harness closes stdin or
//! stops reading stdout — the process lifetime IS the downstream conversation, and the harness
//! owns it.
//!
//! Requests are forwarded CONCURRENTLY (one thread per in-flight message): an agent that issues
//! parallel tool calls would otherwise have them serialized behind the slowest, and a
//! cancellation notification must be able to overtake the call it cancels. The ONE exception is
//! the first message ([`Gate`]): it completes alone, because its response is where a stateful
//! gateway mints the conversation id, and a pipelined burst racing past it would arrive id-less
//! and be refused. Stdout stays line-atomic ([`crate::out::stdout_protocol_line`] writes and
//! flushes under one lock).

use std::io::{BufRead, BufReader, Read};
use std::process::ExitCode;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::fs_seam::RealFs;
use crate::out::errln;
use crate::sessions::{SESSION_ACTIVE, read_sessions};
use crate::sidecar::Layout;

/// The three refusals a relayed request can earn on this side of the wire — FINAL copy.
const NOT_SIGNED_IN: &str = "topos: not signed in on this machine. Run `topos login`.";
const STALE_ENTRY: &str = "topos: this entry belongs to an earlier sign-in. It updates \
                           automatically on the next sweep — or run `topos update`.";
fn unreachable_copy(host: &str) -> String {
    format!("topos: can't reach the gateway at {host}.")
}
const BROKEN_REPLY: &str = "topos: the gateway's reply broke off before it finished.";

/// The JSON-RPC error code every relay-side refusal carries — the server-defined range's first,
/// the same code the gateway's own refusals use, so an agent branches on one number.
const RELAY_ERROR_CODE: i64 = -32000;

/// How long a dial may take before the gateway is called unreachable. Response and body carry NO
/// deadline: a tool call runs as long as the agent is willing to wait, and the agent owns the
/// wait — it can kill this process, which is the cancellation surface a stdio server has.
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// How long the exit waits for in-flight replies after stdin closes. Long enough for any answer
/// that is actually coming; short enough that a wedged upstream cannot hold the process.
const DRAIN_GRACE_SECS: u64 = 30;

/// Serve the conversation until the harness closes stdin. Never fails: every per-message fault is
/// answered in-band, and a torn-down stdout latches quietly ([`crate::out`]).
///
/// Stdin closing IS the shutdown — but not an instant one: a peer that batches its requests and
/// closes stdin still expects the replies, so the exit DRAINS what is in flight, bounded by
/// [`DRAIN_GRACE_SECS`] (an upstream call stuck past that dies with the process — a hang here
/// would hold the exit the protocol promises, and the response deadline is deliberately none).
/// Worker threads are counted, never joined; the parting act is a bounded, best-effort `DELETE`
/// of the gateway conversation this process minted, so a clean exit does not strand session
/// state there.
pub(crate) fn run(home: &std::path::Path, url: &str) -> ExitCode {
    let relay = Arc::new(Relay::new(home.to_path_buf(), url.to_owned()));
    let stdin = std::io::stdin();
    // The FIRST line read is the gate's opener — decided here, in stdin order, because thread
    // scheduling must not let a later message overtake the one whose reply establishes the
    // conversation ([`Gate`]).
    let mut opener = true;
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        if crate::out::stdout_closed() {
            break;
        }
        let relay = Arc::clone(&relay);
        relay.begin();
        let is_opener = std::mem::take(&mut opener);
        std::thread::spawn(move || {
            relay.handle(&line, is_opener);
            relay.finish();
        });
    }
    relay.drain(Duration::from_secs(DRAIN_GRACE_SECS));
    relay.end_conversation();
    ExitCode::SUCCESS
}

/// One protocol line out — written and flushed atomically.
macro_rules! protocol {
    ($($arg:tt)*) => { crate::out::stdout_protocol_line(format_args!($($arg)*)) };
}

struct Relay {
    agent: ureq::Agent,
    /// The gateway address, exactly as the entry names it.
    url: String,
    /// The address's host, for the unreachable sentence.
    host: String,
    /// The `sn_…` path segment — which session this entry was rendered for.
    session_segment: Option<String>,
    /// The `~/.topos` root the session store resolves under (the app's `resolve_home` answer).
    home: std::path::PathBuf,
    /// The conversation id the gateway minted, if it minted one — echoed on every later request.
    mcp_session: Mutex<Option<String>>,
    /// **The first-message gate.** The very first message must COMPLETE before any other is
    /// forwarded: its response is where a stateful gateway mints the conversation id, and a
    /// pipelined burst racing past it would reach the gateway id-less and be refused. Transport
    /// state, not protocol: nothing reads a method — whichever message is first holds the gate,
    /// and every conversation after it flows concurrently.
    gate: Mutex<Gate>,
    opened: Condvar,
    /// How many messages are in flight — the exit drains this to zero (or the grace deadline).
    in_flight: Mutex<usize>,
    drained: Condvar,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// The first message has not completed yet; everything after it waits.
    Held,
    /// The first message completed (however it went); everything flows.
    Open,
}

impl Relay {
    fn new(home: std::path::PathBuf, url: String) -> Self {
        let host = url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or(&url)
            .to_owned();
        let session_segment = url
            .split('/')
            .find(|segment| segment.starts_with("sn_"))
            .map(ToOwned::to_owned);
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_connect(Some(Duration::from_secs(CONNECT_TIMEOUT_SECS)))
                .build(),
        );
        Self {
            agent,
            url,
            host,
            session_segment,
            home,
            mcp_session: Mutex::new(None),
            gate: Mutex::new(Gate::Held),
            opened: Condvar::new(),
            in_flight: Mutex::new(0),
            drained: Condvar::new(),
        }
    }

    /// One more message in flight — counted by the READ loop, before its thread exists, so the
    /// drain can never observe a gap between a line read and its worker starting.
    fn begin(&self) {
        *self.in_flight.lock().expect("poisoned") += 1;
    }

    /// One message done, however it went.
    fn finish(&self) {
        let mut count = self.in_flight.lock().expect("poisoned");
        *count -= 1;
        if *count == 0 {
            self.drained.notify_all();
        }
    }

    /// Wait for the in-flight count to reach zero, or for the grace to run out.
    fn drain(&self, grace: Duration) {
        let deadline = std::time::Instant::now() + grace;
        let mut count = self.in_flight.lock().expect("poisoned");
        while *count > 0 {
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return;
            };
            let (guard, _) = self.drained.wait_timeout(count, left).expect("poisoned");
            count = guard;
        }
    }

    /// Block until the opener completes. The opener never waits — it IS the wait.
    fn wait_open(&self) {
        let mut gate = self.gate.lock().expect("poisoned");
        while *gate != Gate::Open {
            gate = self.opened.wait(gate).expect("poisoned");
        }
    }

    /// The opener's message is done — however it went, the wait is over.
    fn open_gate(&self) {
        *self.gate.lock().expect("poisoned") = Gate::Open;
        self.opened.notify_all();
    }

    /// One inbound message, start to finish: credential, POST, replies out. `opener` is stdin
    /// order, assigned by the read loop — the first message completes alone.
    fn handle(self: &Arc<Self>, line: &str, opener: bool) {
        if !opener {
            self.wait_open();
        }
        self.handle_inner(line);
        if opener {
            self.open_gate();
        }
    }

    fn handle_inner(self: &Arc<Self>, line: &str) {
        // The id is read for ERROR CORRELATION only — the message itself is forwarded verbatim.
        // Absent or null = a notification, which has no reply channel.
        let id = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .filter(|v| !v.is_null());
        let credential = match self.credential() {
            Ok(credential) => credential,
            Err(refusal) => {
                self.refuse(id, refusal);
                return;
            }
        };
        if let Err(refusal) = self.forward(line, &credential, id.as_ref()) {
            self.refuse(id, &refusal);
        }
    }

    /// **The live credential lookup** — the session whose id the URL names, read from the store
    /// at every message so sign-in state is always current.
    fn credential(&self) -> Result<String, &'static str> {
        let fs = RealFs;
        let layout = Layout::new(&self.home);
        let Ok(sessions) = read_sessions(&fs, &layout) else {
            return Err(NOT_SIGNED_IN);
        };
        if let Some(segment) = &self.session_segment
            && let Some(session) = sessions
                .sessions
                .iter()
                .find(|s| s.status == SESSION_ACTIVE && &s.session_id == segment)
        {
            return Ok(session.credential.clone());
        }
        // No session matches the entry. A machine with OTHER live sign-ins holds a stale entry
        // (the sweep rewrites it); a machine with none is simply not signed in.
        if sessions.sessions.iter().any(|s| s.status == SESSION_ACTIVE) {
            Err(STALE_ENTRY)
        } else {
            Err(NOT_SIGNED_IN)
        }
    }

    /// POST one message; write every reply line as it arrives.
    ///
    /// # Errors
    /// The unreachable sentence, or the gateway's own non-protocol answer — for [`Self::refuse`]
    /// to correlate.
    fn forward(
        self: &Arc<Self>,
        line: &str,
        credential: &str,
        id: Option<&Value>,
    ) -> Result<(), String> {
        let mut request = self
            .agent
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", &format!("Bearer {credential}"));
        let session = self.mcp_session.lock().expect("poisoned").clone();
        if let Some(session) = session {
            request = request.header("mcp-session-id", &session);
        }
        let response = request
            .send(line.as_bytes())
            .map_err(|_| unreachable_copy(&self.host))?;
        if let Some(minted) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            self.adopt_conversation(minted);
        }
        let status = response.status().as_u16();
        let event_stream = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.contains("text/event-stream"));
        let reader = response.into_body().into_reader();
        if status >= 400 {
            return self.forward_refusal(reader, status, id);
        }
        let (emitted, whole) = if event_stream {
            stream_events(reader)
        } else {
            let mut body = String::new();
            let whole = BufReader::new(reader).read_to_string(&mut body).is_ok();
            let mut emitted = 0;
            if let Some(framed) = framed(body.trim()) {
                protocol!("{framed}");
                emitted = 1;
            }
            (emitted, whole)
        };
        // A reply that broke off before ANYTHING came out would leave the agent waiting forever;
        // one that broke after its answer line landed is only a diagnostic.
        if !whole && emitted == 0 {
            return Err(BROKEN_REPLY.to_owned());
        }
        if !whole {
            errln!("{BROKEN_REPLY}");
        }
        Ok(())
    }

    /// Record the conversation id the gateway minted — and, THE FIRST time one appears, open the
    /// session's own event stream: the standing authenticated GET a streamable-http client keeps,
    /// where the gateway sends what nobody asked for (`tools/list_changed`, server-initiated
    /// requests). One listener for the process's life, reconnecting quietly, dying with it.
    fn adopt_conversation(self: &Arc<Self>, minted: &str) {
        let mut held = self.mcp_session.lock().expect("poisoned");
        let fresh = held.is_none();
        *held = Some(minted.to_owned());
        drop(held);
        if fresh {
            let relay = Arc::clone(self);
            std::thread::spawn(move || relay.listen());
        }
    }

    /// The standing GET: unwrap server-initiated events onto stdout for as long as the process
    /// lives, reconnecting after a pause when the stream ends. Quiet by design — a gateway that
    /// refuses the stream (an older one, a revoked session) costs nothing but the retry.
    ///
    /// The session id is cloned OUT of its lock before anything dials: a let-chain binding would
    /// keep the guard alive for the whole body, and this body can read an open stream for hours —
    /// with the lock held, every request needing the session id (all of them) would deadlock.
    fn listen(&self) {
        loop {
            if crate::out::stdout_closed() {
                return;
            }
            let session = self.mcp_session.lock().expect("poisoned").clone();
            if let Some(session) = session
                && let Ok(credential) = self.credential()
                && let Ok(response) = self
                    .agent
                    .get(&self.url)
                    .header("accept", "text/event-stream")
                    .header("authorization", &format!("Bearer {credential}"))
                    .header("mcp-session-id", &session)
                    .call()
                && response.status().as_u16() < 400
            {
                stream_events(response.into_body().into_reader());
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    /// The parting act at stdin close: tell the gateway the conversation this process minted is
    /// over, bounded and best-effort — an unreachable gateway must not hold the exit.
    fn end_conversation(&self) {
        let Some(session) = self.mcp_session.lock().expect("poisoned").clone() else {
            return;
        };
        let Ok(credential) = self.credential() else {
            return;
        };
        let farewell = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_connect(Some(Duration::from_secs(3)))
                .timeout_recv_response(Some(Duration::from_secs(3)))
                .timeout_recv_body(Some(Duration::from_secs(3)))
                .build(),
        );
        let _ = farewell
            .delete(&self.url)
            .header("authorization", &format!("Bearer {credential}"))
            .header("mcp-session-id", &session)
            .call();
    }

    /// A refused POST answering a REQUEST: the gateway's own JSON-RPC error is forwarded as the
    /// reply — with the request's id put on it where the gateway answered `null` (its
    /// pre-protocol refusals do), so the agent can correlate it. Anything that is not a JSON-RPC
    /// message becomes one honest sentence naming the status. A refused NOTIFICATION has no reply
    /// channel: its refusal lands on stderr whole, never as an unmatched `id: null` frame on the
    /// protocol stream.
    fn forward_refusal(
        &self,
        reader: impl Read,
        status: u16,
        id: Option<&Value>,
    ) -> Result<(), String> {
        let mut body = String::new();
        let _ = BufReader::new(reader).read_to_string(&mut body);
        if let Ok(mut message) = serde_json::from_str::<Value>(&body)
            && message.get("jsonrpc").is_some()
        {
            let Some(id) = id else {
                let said = message
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("refused");
                errln!("topos: the gateway refused a notification (HTTP {status}: {said}).");
                return Ok(());
            };
            if message.get("id").is_none_or(Value::is_null)
                && let Some(slot) = message.as_object_mut()
            {
                slot.insert("id".to_owned(), id.clone());
            }
            protocol!("{message}");
            return Ok(());
        }
        Err(format!("topos: the gateway answered HTTP {status}."))
    }

    /// The id-matched refusal — or, for a notification, one stderr line (there is no reply
    /// channel to put it on, and stdout carries protocol alone).
    fn refuse(&self, id: Option<Value>, message: &str) {
        match id {
            Some(id) => {
                let error = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": RELAY_ERROR_CODE, "message": message },
                });
                protocol!("{error}");
            }
            None => errln!("{message}"),
        }
    }
}

/// Unwrap an SSE stream as it arrives: each event's data payload becomes one stdout line the
/// moment its blank-line terminator (or the stream's end) is read — a progress notification
/// reaches the agent while the call that caused it still runs. Answers with how many lines it
/// emitted, and whether the stream ended cleanly rather than mid-read.
fn stream_events(reader: impl Read) -> (usize, bool) {
    let mut data: Vec<String> = Vec::new();
    let mut emitted = 0usize;
    let mut emit = |data: &mut Vec<String>| {
        if data.is_empty() {
            return;
        }
        if let Some(framed) = framed(&data.join("\n")) {
            protocol!("{framed}");
            emitted += 1;
        }
        data.clear();
    };
    let mut whole = true;
    for line in BufReader::new(reader).lines() {
        let Ok(line) = line else {
            whole = false;
            break;
        };
        if line.is_empty() {
            emit(&mut data);
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
        }
        // Every other SSE field (`event:`, `id:`, `retry:`, comments) is framing, not payload.
    }
    emit(&mut data);
    (emitted, whole)
}

/// One payload as ONE stdout line — and only ever a JSON one. Newline-framed stdio cannot carry
/// an interior newline, so a payload holding one is re-serialized compact (it is JSON-RPC; key
/// order is not meaning); and bytes that are not JSON at all — a proxy's error page, a truncated
/// frame — are dropped with a diagnostic rather than written, because stdout is protocol alone
/// and one torn frame would desynchronize the whole stream.
fn framed(payload: &str) -> Option<String> {
    if payload.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(payload) {
        Ok(_) if !payload.contains('\n') => Some(payload.to_owned()),
        Ok(value) => Some(value.to_string()),
        Err(_) => {
            errln!("topos: dropped an unframeable reply from the gateway.");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_passes_single_line_json_through() {
        assert_eq!(framed(r#"{"a":1}"#), Some(r#"{"a":1}"#.to_owned()));
        assert_eq!(framed(""), None);
        // Non-JSON bytes — a proxy's error page, a truncated frame — never reach stdout.
        assert_eq!(framed("<html>bad gateway</html>"), None);
        assert_eq!(framed(r#"{"jsonrpc":"2.0","id":1,"resu"#), None);
    }

    #[test]
    fn framed_compacts_multiline_json() {
        let framed = framed("{\n  \"jsonrpc\": \"2.0\",\n  \"id\": 1\n}").expect("compacted");
        assert!(!framed.contains('\n'));
        let value: Value = serde_json::from_str(&framed).expect("still JSON");
        assert_eq!(value["id"], 1);
    }

    #[test]
    fn framed_drops_unframeable_bytes() {
        assert_eq!(framed("not\njson"), None);
    }

    #[test]
    fn relay_reads_the_session_segment_and_host() {
        let relay = Relay::new(
            std::path::PathBuf::from("/nowhere"),
            "https://gateway.example.com/sn_abc123/mcps_def".to_owned(),
        );
        assert_eq!(relay.host, "gateway.example.com");
        assert_eq!(relay.session_segment.as_deref(), Some("sn_abc123"));
    }

    #[test]
    fn relay_survives_a_url_with_no_session_segment() {
        let relay = Relay::new(std::path::PathBuf::from("/nowhere"), "not-a-url".to_owned());
        assert_eq!(relay.host, "not-a-url");
        assert_eq!(relay.session_segment, None);
    }
}
