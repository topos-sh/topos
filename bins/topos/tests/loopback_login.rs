//! The ZERO-TYPING loopback login, proven over the real binary on a real PTY: an interactive
//! `topos login <address>` must START the flow loopback-bound — the write-once declaration the
//! approval page's pre-armed card resolves on — auto-open the browser with the challenge and the
//! listener's return coordinates, keep the short code OFF the terminal, and finish the moment the
//! approval redirect wakes it. No typing anywhere. (The regression this pins: a fresh TTY login
//! once started the flow DEVICE-bound while holding the listener and suppressing the code — the
//! page could not pre-arm, the code was nowhere, and the wait ran out the flow's whole 15-minute
//! TTL.)
//!
//! The second test pins the FRAGMENTATION GUARANTEE the redirect is only an accelerator for: with
//! no redirect at all — the human approved on their phone, or the browser hand-off was lost — the
//! ordinary poll still completes the login. Nothing but the poll ever completes it.
//!
//! The TTY is real: the binary runs under `script(1)` (present on macOS and Linux both), so the
//! interactive browser-open path is the one taken — the exact wiring the piped suite cannot
//! reach. The browser is a PATH-shadowed stub recording the URL it was handed; the fixture is a
//! real loopback HTTP server speaking the login wire, granting once the test has "approved".

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_topos")
}

/// A unique, self-created scratch home for one login (`TOPOS_HOME` AND `HOME` — the granted
/// receipt arms triggers and places the built-in skill, which must never touch the real home).
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("topos-loopback-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// What the fixture server observed: every `/v1/login/authorize` body, every `/v1/login/token`
/// body — the wire truth the assertions read.
#[derive(Default)]
struct Observed {
    authorize_bodies: Vec<String>,
    token_bodies: Vec<String>,
}

/// The login-wire fixture: the constant protocol card at any path, the `/v1/login/authorize`
/// start, and a `/v1/login/token` poll that answers `pending` until the TEST approves — standing
/// in for the human at the browser. The poll is the whole exchange; the client presents nothing
/// but its flow code.
struct Fixture {
    base: String,
    observed: Arc<Mutex<Observed>>,
    approved: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Fixture {
    /// Play the human's part in the browser: from here the poll grants.
    fn approve(&self) {
        self.approved.store(true, Ordering::Relaxed);
    }
}

fn spawn_fixture() -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let port = listener.local_addr().expect("local addr").port();
    let base = format!("http://127.0.0.1:{port}");
    let observed = Arc::new(Mutex::new(Observed::default()));
    let approved = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let (base_thread, observed_thread, approved_thread, stop_thread) = (
        base.clone(),
        Arc::clone(&observed),
        Arc::clone(&approved),
        Arc::clone(&stop),
    );
    std::thread::spawn(move || {
        while !stop_thread.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    handle(stream, &base_thread, &observed_thread, &approved_thread);
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
        observed,
        approved,
        stop,
    }
}

/// Answer one login-wire request; every response closes its connection, so each accept sees one.
fn handle(mut stream: TcpStream, base: &str, observed: &Mutex<Observed>, approved: &AtomicBool) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    // Read the WHOLE request — the headers plus the Content-Length'd body. TCP may split a
    // request across reads, and a partial body here would drop the very fields the assertions
    // branch on (the start's binding declaration; the preselection, or its absence).
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..end]).into_owned();
            let need: usize = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.eq_ignore_ascii_case("content-length") {
                        v.trim().parse().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            if raw.len() >= end + 4 + need {
                break;
            }
        }
        if Instant::now() >= deadline || raw.len() > 65536 {
            break;
        }
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let request = String::from_utf8_lossy(&raw).into_owned();
    let line = request.lines().next().unwrap_or("");
    let body_in = request
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    let body = if line.contains("/v1/login/authorize") {
        observed.lock().unwrap().authorize_bodies.push(body_in);
        format!(
            r#"{{"device_code":"dc_secret","user_code":"WXYZ-3N3X","verification_uri":"{base}/verify","expires_in_secs":900,"interval_secs":3}}"#
        )
    } else if line.contains("/v1/login/token") {
        observed.lock().unwrap().token_bodies.push(body_in);
        if approved.load(Ordering::Relaxed) {
            // Approved in the browser — the poll releases the session's credential and names the
            // workspace the human chose there.
            r#"{"status":"granted","credential":"sess-cred","session_id":"sn_1","session_status":"active","workspace":{"workspace_id":"w_1","name":"eng","display_name":"Engineering"}}"#
                .to_owned()
        } else {
            r#"{"status":"pending"}"#.to_owned()
        }
    } else {
        // Anything else (the card fetch; the granted receipt's best-effort delivery read, which
        // tolerates a miss) gets the constant protocol card.
        format!(r#"{{"schema_version":1,"card":"topos-protocol-card","api_base_url":"{base}"}}"#)
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

/// Write the PATH-shadowing browser stub (both spellings — `open` for macOS, `xdg-open` for a
/// graphical Linux) recording each handed URL into `opened-urls.txt` beside the scratch home.
fn write_browser_stub(home: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let stub_dir = home.join("stub-bin");
    std::fs::create_dir_all(&stub_dir).unwrap();
    let log = home.join("opened-urls.txt");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"{}\"\n",
        log.display()
    );
    for name in ["open", "xdg-open"] {
        let path = stub_dir.join(name);
        std::fs::write(&path, &script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    stub_dir
}

/// Run `topos login <address>` INTERACTIVELY — under `script(1)`, so stdin/stdout/stderr are a
/// real PTY and the browser-open path arms. Returns the child handle; output is collected by the
/// caller after the ceremony completes.
fn spawn_interactive_login(home: &Path, stub_dir: &Path, address: &str) -> std::process::Child {
    let inner = format!("{} login {}", bin(), address);
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("script");
        c.args(["-q", "/dev/null", bin(), "login", address]);
        c
    } else {
        let mut c = Command::new("script");
        c.args(["-qec", &inner, "/dev/null"]);
        c
    };
    let path = format!(
        "{}:{}",
        stub_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    cmd.env("TOPOS_HOME", home)
        .env("HOME", home)
        .env("CLAUDE_CONFIG_DIR", home.join(".claude-isolated"))
        .env("PATH", path)
        // A graphical session on Linux (harmless on macOS, whose opener needs no display).
        .env("DISPLAY", ":0")
        .env("TOPOS_NO_UPDATE_CHECK", "1")
        .env_remove("SSH_CONNECTION")
        .env_remove("SSH_TTY")
        .env_remove("TOPOS_NO_BROWSER")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn().expect("spawn topos under script(1)")
}

/// Wait for a file to appear and return its contents, or panic past the deadline.
fn wait_for_file(path: &Path, deadline: Duration, what: &str) -> String {
    let started = Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(path)
            && !s.trim().is_empty()
        {
            return s;
        }
        assert!(
            started.elapsed() < deadline,
            "{what} never appeared at {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Collect the child's output under a watchdog far below the flow's 15-minute TTL.
fn collect(mut child: std::process::Child, watchdog: Duration) -> Output {
    let started = Instant::now();
    loop {
        if child.try_wait().expect("poll child").is_some() {
            return child.wait_with_output().expect("collect output");
        }
        if started.elapsed() > watchdog {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the interactive login did not exit within {watchdog:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_tty_login_is_codeless_end_to_end() {
    let fx = spawn_fixture();
    let home = scratch("codeless");
    let stub_dir = write_browser_stub(&home);

    let child = spawn_interactive_login(&home, &stub_dir, &fx.base);

    // The browser stub receives the approval URL: the flow's CHALLENGE (a 64-hex digest — never
    // the device code or the short code) plus the listener's return coordinates.
    let opened = wait_for_file(
        &home.join("opened-urls.txt"),
        Duration::from_secs(20),
        "the auto-opened approval URL",
    );
    let url = opened.trim().lines().next().unwrap().to_owned();
    assert!(
        url.starts_with(&format!("{}/verify?", fx.base)),
        "the approval page is the fixture's own verify URL: {url}"
    );
    let query: std::collections::HashMap<&str, &str> = url
        .split_once('?')
        .unwrap()
        .1
        .split('&')
        .filter_map(|p| p.split_once('='))
        .collect();
    let device = query.get("device").copied().expect("a device challenge");
    assert!(
        device.len() == 64 && device.bytes().all(|b| b.is_ascii_hexdigit()),
        "the challenge is a 64-hex digest: {device}"
    );
    assert!(
        !url.contains("dc_secret") && !url.contains("WXYZ-3N3X"),
        "no secret and no code rides the URL: {url}"
    );
    let port: u16 = query
        .get("port")
        .and_then(|p| p.parse().ok())
        .expect("the listener port");
    let state = query.get("state").copied().expect("the ceremony state");

    // THE START — the regression's exact seam: this run must have declared the flow
    // loopback-bound, or the approval page has nothing to pre-arm and the suppressed code strands
    // the human. And it names a SERVER only: the workspace is chosen in the browser, so nothing
    // about one rides this unauthenticated start.
    {
        let obs = fx.observed.lock().unwrap();
        assert_eq!(obs.authorize_bodies.len(), 1, "one flow start");
        assert!(
            obs.authorize_bodies[0].contains(r#""redirect":"loopback""#),
            "the start declares the loopback binding: {}",
            obs.authorize_bodies[0]
        );
        assert!(
            !obs.authorize_bodies[0].contains("workspace")
                && !obs.authorize_bodies[0].contains("preselect"),
            "a bare server address preselects nothing: {}",
            obs.authorize_bodies[0]
        );
    }

    // Play the approval page's part: the human approves, and the state-bound redirect wakes the
    // CLI's own 127.0.0.1 listener. The redirect carries NO secret — it only says "poll now".
    fx.approve();
    let mut redirect = TcpStream::connect(("127.0.0.1", port)).expect("dial the CLI's listener");
    redirect
        .write_all(
            format!("GET /cb?state={state}&outcome=approved HTTP/1.1\r\nhost: x\r\n\r\n")
                .as_bytes(),
        )
        .expect("write the redirect");
    let mut answer = String::new();
    let _ = redirect.read_to_string(&mut answer);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");

    // The redirect wakes the poll; the poll completes the login.
    let out = collect(child, Duration::from_secs(60));
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "the login exits 0: {printed}");
    assert!(
        printed.contains("Opening your browser to approve."),
        "the auto-open is disclosed: {printed}"
    );
    // CODELESS TO OPERATE, glance-checkable to trust: the loopback arm never prints the
    // "Open:/Code:" instruction pair (nothing asks for typing), but the WAITING LINE carries the
    // code — the human-verifiable cross-check against what the page shows, now that an approval
    // can arrive from anywhere. Static, timer-free (the device-flow precedent waits quietly).
    assert!(
        !printed.contains("Code:"),
        "no typing instruction on the loopback arm: {printed}"
    );
    assert!(
        printed.contains(
            "Waiting for approval in your browser — code WXYZ-3N3X (the page shows the same code)."
        ),
        "the waiting line carries the glance code: {printed}"
    );
    assert!(
        !printed.contains("elapsed") && !printed.contains("expires in"),
        "no timer fragments on the wait: {printed}"
    );
    // The poll carries the flow code and NOTHING else — the redirect brought no second secret.
    {
        let obs = fx.observed.lock().unwrap();
        let last = obs.token_bodies.last().expect("at least one poll");
        assert!(last.contains("dc_secret"), "the flow code travels: {last}");
        assert!(
            !last.contains("auth_code"),
            "no second secret rides the poll: {last}"
        );
    }
    // The session row landed: the receipt's workspace, the fixture's credential.
    let sessions =
        std::fs::read_to_string(home.join("identity/sessions.json")).expect("the session doc");
    assert!(
        sessions.contains("sess-cred") && sessions.contains("w_1"),
        "the granted session persists: {sessions}"
    );
    assert!(
        !home.join("identity/enrollment.json").exists(),
        "the WAL dies with the grant"
    );
}

#[test]
fn a_login_completes_on_the_poll_when_no_redirect_ever_arrives() {
    // The fragmentation guarantee: the redirect is an ACCELERATOR. Approve somewhere this machine
    // cannot be redirected from — a phone, another laptop — and the ordinary poll interval still
    // finishes the login. Nothing here dials the CLI's listener at all.
    let fx = spawn_fixture();
    let home = scratch("no-redirect");
    let stub_dir = write_browser_stub(&home);

    let child = spawn_interactive_login(&home, &stub_dir, &fx.base);
    wait_for_file(
        &home.join("opened-urls.txt"),
        Duration::from_secs(20),
        "the auto-opened approval URL",
    );
    fx.approve();

    let out = collect(child, Duration::from_secs(60));
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "the login exits 0: {printed}");
    assert!(
        printed.contains("Connected to Engineering"),
        "the connected receipt prints: {printed}"
    );
    let sessions =
        std::fs::read_to_string(home.join("identity/sessions.json")).expect("the session doc");
    assert!(
        sessions.contains("sess-cred") && sessions.contains("w_1"),
        "the poll alone persisted the session: {sessions}"
    );
    assert!(
        !home.join("identity/enrollment.json").exists(),
        "the WAL dies with the grant"
    );
}
