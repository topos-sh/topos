//! The PIPED login default, proven over the real binary: an agent harness surfaces a command's
//! output only when it exits, so a piped `topos login [<address>]` (no `--json`, no `--wait`) must
//! NEVER sit in the human blocking wait — it prints the approval instructions, performs its
//! invocation's one poll, and exits 0 with the WAL pending for the next re-invoke.
//! `--wait <seconds>` stays the explicit piped opt-in, capped at its deadline. A BARE `topos
//! login` — the headline usage — runs the same way against the default server.
//!
//! The fixture is a real loopback HTTP server (std `TcpListener`, one thread) answering the
//! constant protocol card, the `/v1/login/authorize` start, and an always-`pending`
//! `/v1/login/token` poll — the exact wire the binary dials; the binary itself runs as a child
//! with PIPED stdio (the point of the test), fenced by a watchdog far under the ~15-minute code
//! TTL a blocking default would have waited out.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_topos")
}

/// A unique, self-created scratch home for one login (`TOPOS_HOME`).
fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::AtomicU32;
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("topos-piped-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The loopback login fixture: serves the constant protocol card at any path, the
/// `/v1/login/authorize` start, and an always-`pending` token poll. Returns the base URL, a live
/// poll counter, every request's `User-Agent`, and a stop flag the test sets on teardown.
struct Fixture {
    base: String,
    polls: Arc<AtomicUsize>,
    /// The `User-Agent` of every request the fixture answered — how a server tells which client
    /// it is talking to, and the only thing a client floor can be held by.
    agents: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
}

fn spawn_fixture() -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let port = listener.local_addr().expect("local addr").port();
    let base = format!("http://127.0.0.1:{port}");
    let polls = Arc::new(AtomicUsize::new(0));
    let agents = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (base_for_card, polls_thread, agents_thread, stop_thread) = (
        base.clone(),
        Arc::clone(&polls),
        Arc::clone(&agents),
        Arc::clone(&stop),
    );
    std::thread::spawn(move || {
        while !stop_thread.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    handle(stream, &base_for_card, &polls_thread, &agents_thread);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    Fixture {
        base,
        polls,
        agents,
        stop,
    }
}

/// Answer one login-wire request. Every response carries `Connection: close`, so ureq opens a
/// fresh connection per request and each accept sees exactly one.
fn handle(mut stream: TcpStream, base: &str, polls: &AtomicUsize, agents: &Mutex<Vec<String>>) {
    // A stream accepted from a non-blocking listener inherits the flag on macOS — force it
    // blocking so the read waits for the request bytes instead of racing to an empty read.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..n]);
    let line = request.lines().next().unwrap_or("");
    if let Some(ua) = request.lines().find_map(|l| {
        let (name, value) = l.split_once(':')?;
        name.eq_ignore_ascii_case("user-agent")
            .then(|| value.trim().to_owned())
    }) {
        agents.lock().expect("record the user agent").push(ua);
    }
    let body = if line.contains("/v1/login/authorize") {
        // The login start: a short poll interval keeps a --wait run brisk.
        format!(
            r#"{{"device_code":"dc_secret","user_code":"WXYZ-3N3X","verification_uri":"{base}/verify","expires_in_secs":900,"interval_secs":1}}"#
        )
    } else if line.contains("/v1/login/token") {
        polls.fetch_add(1, Ordering::Relaxed);
        r#"{"status":"pending"}"#.to_owned()
    } else {
        // Any other path is the constant protocol card (the bare-origin card fetch); the base it
        // re-roots onto is this same server.
        format!(
            // Card declarations included, as a real server serves them: this fake speaks the
            // version the client under test is built from, and names the client floor it holds.
            r#"{{"schema_version":1,"card":"topos-protocol-card","api_base_url":"{base}","server_version":"{}","min_cli_version":"0.1.15"}}"#,
            env!("CARGO_PKG_VERSION")
        )
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Run `topos <args>` as a CHILD with PIPED stdio (the point: stdout is not a TTY), under a
/// watchdog far below the code's ~15-minute TTL. Returns the captured output, or panics if the
/// child overran the watchdog (a blocking default would have).
fn run_piped(home: &Path, base: &str, args: &[&str], watchdog: Duration) -> Output {
    let mut child = Command::new(bin())
        .env("TOPOS_HOME", home)
        .env("CLAUDE_CONFIG_DIR", home.join(".claude-isolated"))
        .env("TOPOS_PLANE_URL", base)
        .env("TOPOS_NO_BROWSER", "1")
        .env_remove("SSH_CONNECTION")
        .env_remove("SSH_TTY")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn topos");
    let started = Instant::now();
    loop {
        if let Some(_status) = child.try_wait().expect("poll child") {
            return child.wait_with_output().expect("collect output");
        }
        if started.elapsed() > watchdog {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "`topos {}` did not exit within {watchdog:?} — the piped default is blocking",
                args.join(" ")
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_piped_login_prints_the_instructions_and_exits_without_the_blocking_wait() {
    let fx = spawn_fixture();
    let home = scratch("begin");

    // Call 1 — begin: card → authorize → WAL. Piped, no --wait → returns promptly (well under the
    // 30s watchdog; a blocking default would have sat out the full ~15-minute code TTL).
    let out = run_piped(
        &home,
        &fx.base,
        &["login", &fx.base],
        Duration::from_secs(30),
    );
    assert!(
        out.status.success(),
        "begin exits 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The approval instructions print — the two-line shape (URL, then the code). A piped,
    // non-`--json` run returns the pending state without entering the blocking wait, so the
    // instructions ride the ordinary TTY render on STDOUT (the blocking wait's stderr disclosure
    // only fires when it actually blocks).
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        printed.contains("Open:"),
        "the approval URL prints: {printed}"
    );
    assert!(printed.contains("Code:"), "the code prints: {printed}");
    assert!(
        printed.contains("WXYZ-3N3X"),
        "the fixture's code is shown: {printed}"
    );
    // The pending WAL is persisted for the re-invoke.
    assert!(
        home.join("identity/enrollment.json").exists(),
        "the login WAL is on disk"
    );
    // Begin performs the authorize, not a token poll — the poll is the resume's job.
    assert_eq!(fx.polls.load(Ordering::Relaxed), 0, "begin does not poll");
    // EVERY session-lane request names this build — a server holds its client floor by this
    // header, so a silent transport is a silent floor.
    let agents = fx.agents.lock().expect("read the recorded agents").clone();
    let expected = format!("topos/{}", env!("CARGO_PKG_VERSION"));
    assert!(agents.len() >= 2, "the card + the authorize both landed");
    assert!(
        agents.iter().all(|ua| *ua == expected),
        "every request identifies itself as {expected}: {agents:?}"
    );

    // Re-invoke — resume: a SINGLE poll, still pending, exits promptly again (never a blocking loop).
    let out = run_piped(&home, &fx.base, &["login"], Duration::from_secs(30));
    assert!(
        out.status.success(),
        "resume exits 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fx.polls.load(Ordering::Relaxed),
        1,
        "the resume polls exactly once"
    );
    assert!(
        home.join("identity/enrollment.json").exists(),
        "still pending — the WAL survives"
    );
}

#[test]
fn a_bare_login_starts_against_the_default_server() {
    let fx = spawn_fixture();
    let home = scratch("bare");

    // No address at all: the default server (here `TOPOS_PLANE_URL`), and NOTHING preselected —
    // the workspace is chosen in the browser, so the CLI names none.
    let out = run_piped(&home, &fx.base, &["login"], Duration::from_secs(30));
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "bare login exits 0: {printed}");
    assert!(
        printed.contains("choose your workspace in the browser"),
        "the receipt says where the workspace comes from: {printed}"
    );
    assert!(
        printed.contains("Open:") && printed.contains("WXYZ-3N3X"),
        "the approval instructions print: {printed}"
    );
    let wal = std::fs::read_to_string(home.join("identity/enrollment.json"))
        .expect("the login WAL is on disk");
    assert!(
        !wal.contains("preselect"),
        "a bare login preselects nothing: {wal}"
    );
    assert_eq!(fx.polls.load(Ordering::Relaxed), 0, "begin does not poll");
}

#[test]
fn an_explicit_wait_cap_blocks_piped_then_returns_at_the_deadline() {
    let fx = spawn_fixture();
    let home = scratch("wait");

    // `--wait 1` is the explicit piped opt-in: it blocks and re-polls, but the 1-second cap ends it
    // far under the watchdog (never the ~15-minute code TTL). The always-pending fixture means it
    // polls at least once past the deadline before returning.
    let started = Instant::now();
    let out = run_piped(
        &home,
        &fx.base,
        &["login", &fx.base, "--wait", "1"],
        Duration::from_secs(30),
    );
    let elapsed = started.elapsed();
    assert!(
        out.status.success(),
        "the capped wait exits 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // It actually blocked (past the 1s cap) rather than returning instantly like the no-wait
    // default, and it did poll while blocking.
    assert!(
        elapsed >= Duration::from_millis(900),
        "the --wait cap blocked for its ~1s: {elapsed:?}"
    );
    assert!(
        fx.polls.load(Ordering::Relaxed) >= 1,
        "the blocking wait re-polled at least once"
    );
    // Still pending — the WAL survives for the next re-invoke.
    assert!(home.join("identity/enrollment.json").exists());
}
