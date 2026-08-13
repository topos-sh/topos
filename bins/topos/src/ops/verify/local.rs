//! **The program arm** — the stdio conversation one verification holds with a server the bundle
//! says every machine RUNS.
//!
//! Same depth as the address arm: the server is asked for its tools, and the answer is what the
//! verdict states. The sequence differs because the transport does.
//!
//! ## Why stdio probes and HTTP does not
//!
//! On HTTP every request comes back with a status code, so a modern request that a legacy server
//! cannot serve is answered — and the answer is the era signal. Over pipes there is no such thing.
//! The specification's stdio binding therefore prescribes the opposite opening: send
//! `server/discover` FIRST, because *some legacy servers do not validate that a request arrives
//! after `initialize` and would process an era-ambiguous method under legacy semantics* — probing
//! yields a deterministic failure instead. Its three outcomes are the ones implemented here: a
//! discovery result means modern; a recognized modern error means modern at another version; **any
//! other error, or no answer at all, means legacy** — and the fallback is deliberately keyed to
//! neither a single error code nor to silence alone, because legacy servers do both.
//!
//! The extra round trip is process-local: both requests travel down the same pipe to the same
//! already-running child, so it costs a write and a read, not a connection.
//!
//! ## The child
//!
//! - **A MINIMAL environment.** The verifier's own environment is NOT inherited. The child gets
//!   `PATH`, `HOME`, `TMPDIR` (plus, on Windows, the handful without which `cmd.exe` and node
//!   cannot start at all), every literal the entry states, and — by NAME — the machine's value for
//!   each slot the entry leaves for this machine to fill. Nothing else. A verification runs a
//!   program a bundle named; it must not hand that program the credentials of whoever ran topos.
//! - **Both output streams are drained concurrently**, on their own threads, so a server that
//!   writes a large log to stderr cannot fill a pipe and deadlock the conversation on stdout.
//!   `stderr` is not evidence of failure (the binding says so explicitly) — a bounded tail of it is
//!   kept only to make "it never started" say WHY.
//! - **A hard deadline, then kill and reap.** Every read is bounded; on the way out the child is
//!   killed and waited for, so no verification can leave a process behind. The reader threads are
//!   deliberately NOT joined: a server that handed its pipes to a grandchild would never reach EOF,
//!   and a join there would hang the verb the deadline exists to protect. (On Windows the same
//!   grandchild survives the kill — terminating a whole process tree needs a Job Object, which this
//!   build does not create; the note stands where the fix would go.)

use std::io::{BufRead, BufReader, Read as _, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::Value;
use topos_harness::mcp::EnvValue;

use super::wire::{
    self, LEGACY_REVISION, METHOD_DISCOVER, METHOD_TOOLS_LIST, MODERN_REVISION, Reply, Verdict,
};

/// The whole conversation's ceiling, from spawn to verdict.
const TOTAL_BUDGET: Duration = Duration::from_secs(20);
/// How long the era probe waits before concluding the child is a handshake-era server. Short: a
/// legacy server's answer to an unknown method is an error it sends immediately, or nothing at all.
const PROBE_BUDGET: Duration = Duration::from_secs(5);
/// The most stdout bytes one line may carry before the read gives up on it. A JSON-RPC message is
/// small; past this the child is streaming something else down the wrong pipe.
const MAX_LINE_BYTES: u64 = 1024 * 1024;
/// How much of the child's stderr is kept to explain a failure to start.
const MAX_STDERR_TAIL: usize = 400;

/// The JSON-RPC ids, one per request in the conversation.
const ID_DISCOVER: u64 = 1;
const ID_MODERN_TOOLS: u64 = 2;
const ID_LEGACY_INIT: u64 = 3;
const ID_LEGACY_TOOLS: u64 = 4;

// =================================================================================================
// The environment policy
// =================================================================================================

/// The variables a child gets from the machine regardless of what the entry says — without which
/// nothing runs at all.
const BASE_ENV: [&str; 3] = ["PATH", "HOME", "TMPDIR"];
/// The Windows additions. `cmd.exe` refuses to start without `SYSTEMROOT`, and node resolves its
/// own installation through the profile/appdata pair — a "minimal" environment that dropped these
/// would turn every Windows verification into a false "not reachable".
const WINDOWS_ENV: [&str; 8] = [
    "SYSTEMROOT",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "TEMP",
    "TMP",
    "APPDATA",
    "LOCALAPPDATA",
];

/// **The whole environment the child is given** — pure, so the policy is a tested outcome rather
/// than a property of whoever runs the suite.
///
/// In order: the base names above (the Windows additions only where they apply), then every slot
/// the entry states. An entry slot WINS over a base name of the same spelling, because the bundle
/// said so on purpose. An [`EnvValue::Inherited`] slot is resolved from the machine BY NAME and
/// **omitted entirely when this machine has no value** — never set to the empty string, which a
/// server checking whether a variable is set would read as a yes.
pub(crate) fn spawn_env(
    entry: &[(String, EnvValue)],
    host: &dyn Fn(&str) -> Option<String>,
    windows: bool,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut put = |name: &str, value: String| match out.iter_mut().find(|(n, _)| n == name) {
        Some(slot) => slot.1 = value,
        None => out.push((name.to_owned(), value)),
    };
    for name in BASE_ENV {
        if let Some(value) = host(name) {
            put(name, value);
        }
    }
    if windows {
        for name in WINDOWS_ENV {
            if let Some(value) = host(name) {
                put(name, value);
            }
        }
    }
    for (name, value) in entry {
        match value {
            EnvValue::Literal(v) => put(name, v.clone()),
            EnvValue::Inherited => {
                if let Some(v) = host(name) {
                    put(name, v);
                }
            }
        }
    }
    out
}

/// The real machine's answer for [`spawn_env`].
fn machine_env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

// =================================================================================================
// The conversation
// =================================================================================================

/// **Verify the program `command` + `args`, run with `env`.**
pub(crate) fn probe(command: &str, args: &[String], env: &[(String, EnvValue)]) -> Verdict {
    let deadline = Instant::now() + TOTAL_BUDGET;
    let mut child = match spawn(command, args, env) {
        Ok(c) => c,
        Err(e) => {
            return Verdict::NotReachable {
                reason: spawn_failure(command, &e),
            };
        }
    };
    let verdict = converse(&mut child, deadline);
    // Killed and reaped on the way out, whatever happened above.
    child.finish();
    verdict
}

/// Why the program never started, in plain words. The two kinds a person can act on say what to do
/// about them; anything else names the operating system's own reason rather than inventing one.
fn spawn_failure(command: &str, e: &std::io::Error) -> String {
    use std::io::ErrorKind as K;
    match e.kind() {
        K::NotFound => format!("{command} is not on this machine"),
        K::PermissionDenied => format!("{command} is on this machine but this user cannot run it"),
        _ => format!("{command} could not be started on this machine ({e})"),
    }
}

/// A spawned server and its drained streams.
struct Running {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    lines: Receiver<String>,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
}

impl Running {
    /// Close stdin (the binding's own graceful-shutdown signal), then kill and reap. `wait` is what
    /// keeps a verification from leaving a zombie; `kill` on an already-exited child is harmless.
    fn finish(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// The bounded tail of what the child wrote to stderr — a diagnostic, never a verdict.
    fn stderr_tail(&self) -> String {
        let held = self.stderr.lock().map(|s| s.clone()).unwrap_or_default();
        wire::clip(held.trim(), MAX_STDERR_TAIL)
    }
}

/// Spawn the server with the minimal environment, both output streams piped.
fn spawn(command: &str, args: &[String], env: &[(String, EnvValue)]) -> std::io::Result<Running> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in spawn_env(env, &machine_env, cfg!(windows)) {
        cmd.env(name, value);
    }
    let mut child = cmd.spawn()?;
    let stdin = child.stdin.take();
    let (tx, lines) = mpsc::channel();
    if let Some(out) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(out).take(MAX_LINE_BYTES * 64);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if tx.send(std::mem::take(&mut line)).is_err() {
                    return;
                }
            }
        });
    }
    let stderr = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(err) = child.stderr.take() {
        let sink = std::sync::Arc::clone(&stderr);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(err).take(MAX_LINE_BYTES);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if let Ok(mut held) = sink.lock() {
                    // A rolling tail: only the last words are ever shown, and the buffer never
                    // grows past what the read itself is capped at.
                    held.push_str(&line);
                    let over = held.chars().count().saturating_sub(MAX_STDERR_TAIL * 2);
                    if over > 0 {
                        *held = held.chars().skip(over).collect();
                    }
                }
                line.clear();
            }
        });
    }
    Ok(Running {
        child,
        stdin,
        lines,
        stderr,
    })
}

/// The three-step conversation, era-probed first.
fn converse(child: &mut Running, deadline: Instant) -> Verdict {
    // 1. The era probe.
    if let Err(v) = send(child, &wire::modern_request(ID_DISCOVER, METHOD_DISCOVER)) {
        return v;
    }
    let probe_deadline = deadline.min(Instant::now() + PROBE_BUDGET);
    match await_reply(child, ID_DISCOVER, probe_deadline) {
        // A discovery result: the peer is modern. Its version list decides where to go next.
        Ok(Reply::Result(result)) => match modern_route(&result) {
            Route::Modern => modern_tools(child, deadline),
            Route::Legacy => legacy(child, deadline),
            Route::Unnegotiable(supported) => wire::verdict_unsupported_version(&supported),
        },
        // A recognized modern error means modern-but-not-at-this-version, over the ONE
        // renegotiation rule both transports read; anything else is the handshake era saying so in
        // its own words.
        Ok(Reply::Error(e)) if e.is_modern() => match wire::renegotiation(&e) {
            wire::Renegotiation::Handshake => legacy(child, deadline),
            wire::Renegotiation::Refused => Verdict::NotMcp { detail: e.detail() },
            wire::Renegotiation::Unnegotiable => wire::verdict_unsupported_version(&e.supported),
        },
        Ok(Reply::Error(_)) => legacy(child, deadline),
        // No answer within the probe's budget, or the child ended: both are legacy signals — a
        // handshake-era server may simply ignore what it cannot parse. A child that has ENDED,
        // though, has nothing left to ask.
        Err(Silence::Timeout) => legacy(child, deadline),
        Err(Silence::Ended) => Verdict::NotReachable {
            reason: ended_reason(child),
        },
        Err(Silence::Unreadable(detail)) => Verdict::NotMcp { detail },
    }
}

/// Where a discovery result sends the conversation next.
enum Route {
    Modern,
    Legacy,
    Unnegotiable(Vec<String>),
}

/// Read a `DiscoverResult`'s version list. A server that named none is taken at its word that it
/// answered a modern request — it did, or it would not have produced a result for one.
fn modern_route(result: &Value) -> Route {
    let Some(versions) = wire::discover_versions(result) else {
        return Route::Modern;
    };
    if versions.is_empty() || versions.iter().any(|v| v == MODERN_REVISION) {
        return Route::Modern;
    }
    if versions.iter().any(|v| wire::speaks_handshake_era(v)) {
        return Route::Legacy;
    }
    Route::Unnegotiable(versions)
}

/// The modern listing — one more request down the same pipe.
fn modern_tools(child: &mut Running, deadline: Instant) -> Verdict {
    if let Err(v) = send(
        child,
        &wire::modern_request(ID_MODERN_TOOLS, METHOD_TOOLS_LIST),
    ) {
        return v;
    }
    match await_reply(child, ID_MODERN_TOOLS, deadline) {
        Ok(reply) => wire::verdict_of_tools(&reply, MODERN_REVISION),
        Err(silence) => silence.verdict(child),
    }
}

/// The handshake era: `initialize` → `notifications/initialized` → `tools/list`.
fn legacy(child: &mut Running, deadline: Instant) -> Verdict {
    if let Err(v) = send(child, &wire::legacy_initialize(ID_LEGACY_INIT)) {
        return v;
    }
    let negotiated = match await_reply(child, ID_LEGACY_INIT, deadline) {
        Ok(Reply::Result(result)) => result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .unwrap_or(LEGACY_REVISION)
            .to_owned(),
        Ok(Reply::Error(e)) => return Verdict::NotMcp { detail: e.detail() },
        Err(silence) => return silence.verdict(child),
    };
    // A notification: written, never answered.
    if let Err(v) = send(child, &wire::legacy_initialized()) {
        return v;
    }
    if let Err(v) = send(child, &wire::legacy_tools_list(ID_LEGACY_TOOLS)) {
        return v;
    }
    match await_reply(child, ID_LEGACY_TOOLS, deadline) {
        Ok(reply) => wire::verdict_of_tools(&reply, &negotiated),
        Err(silence) => silence.verdict(child),
    }
}

/// Write one message as a single newline-terminated line — the binding's framing, and the reason a
/// message may never contain an embedded newline.
///
/// # Errors
/// The verdict a closed pipe earns: the child is gone, which is a not-reachable answer, not a
/// protocol one.
fn send(child: &mut Running, message: &Value) -> Result<(), Verdict> {
    let Some(stdin) = child.stdin.as_mut() else {
        return Err(Verdict::NotReachable {
            reason: ended_reason(child),
        });
    };
    let mut line = serde_json::to_vec(message).unwrap_or_else(|_| b"{}".to_vec());
    line.push(b'\n');
    stdin
        .write_all(&line)
        .and_then(|()| stdin.flush())
        .map_err(|_| Verdict::NotReachable {
            reason: ended_reason(child),
        })
}

/// Why no reply came.
enum Silence {
    /// The budget ran out with the child still alive.
    Timeout,
    /// The child closed its stdout (it exited, or it will).
    Ended,
    /// A line arrived, and it was not a reply this conversation can use.
    Unreadable(String),
}

impl Silence {
    /// The verdict a silence earns once the conversation has already established the peer answers.
    fn verdict(self, child: &Running) -> Verdict {
        match self {
            Self::Timeout => Verdict::NotReachable {
                reason: format!(
                    "the program did not answer within {}s",
                    TOTAL_BUDGET.as_secs()
                ),
            },
            Self::Ended => Verdict::NotReachable {
                reason: ended_reason(child),
            },
            Self::Unreadable(detail) => Verdict::NotMcp { detail },
        }
    }
}

/// The sentence for a child that is gone — with the tail of whatever it said on its way out, which
/// is usually the whole explanation ("Cannot find module …").
fn ended_reason(child: &Running) -> String {
    let tail = child.stderr_tail();
    if tail.is_empty() {
        "the program ended without answering".to_owned()
    } else {
        format!("the program ended without answering ({tail})")
    }
}

/// Read lines until one is the reply to `id`, the child's stdout ends, or `deadline` passes.
///
/// A line that is not this request's reply is SKIPPED, not fatal: the binding says notifications
/// share the channel, and a reply to an earlier request may still be in flight. A line that is not
/// JSON at all is skipped too — servers are told not to write anything else to stdout, and the ones
/// that do (a banner, a warning) should not turn a healthy server into a verdict.
fn await_reply(child: &Running, id: u64, deadline: Instant) -> Result<Reply, Silence> {
    let mut saw_something = false;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Silence::Timeout);
        }
        match child.lines.recv_timeout(remaining) {
            Ok(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                saw_something = true;
                if let Ok(reply) = wire::correlate("application/json", line.trim(), id) {
                    return Ok(reply);
                }
            }
            Err(RecvTimeoutError::Timeout) => return Err(Silence::Timeout),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(if saw_something {
                    Silence::Unreadable(
                        "it wrote to its output, but never the reply to this request".to_owned(),
                    )
                } else {
                    Silence::Ended
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(name: &str, value: EnvValue) -> (String, EnvValue) {
        (name.to_owned(), value)
    }

    /// A fixed machine environment for the pure policy.
    fn machine(name: &str) -> Option<String> {
        match name {
            "PATH" => Some("/usr/bin".to_owned()),
            "HOME" => Some("/home/ada".to_owned()),
            "TMPDIR" => Some("/tmp".to_owned()),
            "SYSTEMROOT" => Some("C:\\Windows".to_owned()),
            "COMSPEC" => Some("C:\\Windows\\cmd.exe".to_owned()),
            "WEATHER_TOKEN" => Some("from-the-machine".to_owned()),
            // The one a person would be horrified to see travel.
            "AWS_SECRET_ACCESS_KEY" => Some("nobody-should-see-this".to_owned()),
            _ => None,
        }
    }

    #[test]
    fn the_child_gets_the_base_names_the_entrys_literals_and_nothing_else() {
        let entry = [
            slot("REGION", EnvValue::Literal("eu-west-1".to_owned())),
            slot("WEATHER_TOKEN", EnvValue::Inherited),
        ];
        let env = spawn_env(&entry, &machine, false);
        let names: Vec<&str> = env.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["PATH", "HOME", "TMPDIR", "REGION", "WEATHER_TOKEN"]);
        // The inherited slot resolves by NAME from this machine.
        assert_eq!(
            env.iter().find(|(n, _)| n == "WEATHER_TOKEN").unwrap().1,
            "from-the-machine"
        );
        // Nothing the verifier happens to hold travels uninvited.
        assert!(
            !env.iter().any(|(n, _)| n == "AWS_SECRET_ACCESS_KEY"),
            "{env:?}"
        );
    }

    #[test]
    fn an_inherited_slot_this_machine_cannot_fill_is_omitted_not_emptied() {
        let entry = [slot("MISSING_ONE", EnvValue::Inherited)];
        let env = spawn_env(&entry, &machine, false);
        assert!(
            !env.iter().any(|(n, _)| n == "MISSING_ONE"),
            "an empty string would read as 'set' to the server: {env:?}"
        );
    }

    #[test]
    fn an_entry_literal_wins_over_a_base_name_of_the_same_spelling() {
        let entry = [slot("PATH", EnvValue::Literal("/opt/bin".to_owned()))];
        let env = spawn_env(&entry, &machine, false);
        assert_eq!(env.iter().filter(|(n, _)| n == "PATH").count(), 1);
        assert_eq!(env[0], ("PATH".to_owned(), "/opt/bin".to_owned()));
    }

    #[test]
    fn windows_adds_exactly_the_names_without_which_nothing_starts() {
        let unix = spawn_env(&[], &machine, false);
        assert_eq!(unix.len(), 3);
        let windows = spawn_env(&[], &machine, true);
        let names: Vec<&str> = windows.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"SYSTEMROOT"), "{names:?}");
        assert!(names.contains(&"COMSPEC"), "{names:?}");
        // Only the ones this machine actually has — the policy never invents a value.
        assert!(!names.contains(&"PATHEXT"), "{names:?}");
    }

    #[test]
    fn a_program_that_does_not_exist_is_not_reachable_and_says_so() {
        let v = probe("topos-no-such-program-anywhere", &[], &[]);
        assert_eq!(v.exit_code(), 4);
        assert_eq!(
            v.line(),
            "not reachable: topos-no-such-program-anywhere is not on this machine"
        );
    }
}
