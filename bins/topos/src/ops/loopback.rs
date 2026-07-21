//! The LOOPBACK approval listener — zero-typing browser enrollment.
//!
//! When a local browser is available, the enrollment wait binds an ephemeral `127.0.0.1`
//! listener, auto-opens the approval page (carrying the flow's device-code CHALLENGE — the hex
//! of its SHA-256, the same value the server keys the flow row by, so the page resolves the
//! request with zero typing; the short code itself never rides a URL), and receives the outcome
//! as ONE state-bound single-use localhost redirect. The redirect carries NOTHING sensitive —
//! `state` (minted per ceremony, matched exactly, spent on first receipt) and the outcome word —
//! and the CLI's ordinary `device/token` POLL stays the source of truth: a lost or spoofed
//! redirect changes nothing but the wake-up latency. The typed-code flow stays the SSH/headless
//! fallback, chosen automatically ([`choose_browser`]).
//!
//! `std::net::TcpListener` only — no server crate enters the client graph.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::time::Duration;

/// What the localhost redirect reported. Advisory: the poll re-verifies either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopbackOutcome {
    Approved,
    Denied,
}

/// The ephemeral single-ceremony listener: `127.0.0.1:0`, non-blocking, one matching redirect.
pub(crate) struct LoopbackListener {
    listener: TcpListener,
    port: u16,
    state: String,
}

impl LoopbackListener {
    /// Bind on an ephemeral loopback port. `state` is the ceremony's one-time binding token
    /// (minted by the caller; matched exactly by [`Self::try_receive`]).
    pub(crate) fn bind(state: String) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            port,
            state,
        })
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// Drain any waiting connections without blocking; answer each; return the outcome of the
    /// FIRST request whose `state` matches (spending it — the caller stops listening). Anything
    /// else (a stray probe, a wrong state, a favicon fetch) gets a plain 404 and keeps nothing.
    pub(crate) fn try_receive(&self) -> Option<LoopbackOutcome> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(outcome) = answer_connection(stream, &self.state) {
                        return Some(outcome);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return None,
                Err(_) => return None,
            }
        }
    }
}

/// Read one HTTP request off the accepted connection, answer it, and return the outcome when it
/// is the ceremony's own redirect. Faults are swallowed — the poll is the truth, never this.
fn answer_connection(mut stream: TcpStream, state: &str) -> Option<LoopbackOutcome> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let mut buf = [0u8; 2048];
    let n = stream.read(&mut buf).ok()?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    match parse_callback(path, state) {
        Some(outcome) => {
            let body = match outcome {
                LoopbackOutcome::Approved => {
                    "You're set — this device is approved. Return to your terminal."
                }
                LoopbackOutcome::Denied => "Request denied — nothing was connected.",
            };
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
            Some(outcome)
        }
        None => {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 0\r\n\r\n",
            );
            None
        }
    }
}

/// Parse a redirect path (`/cb?state=…&outcome=approved|denied`) against the ceremony's state.
/// Pure — unit-tested. A missing/mismatched state or an unknown outcome is `None` (404'd, kept
/// listening): the state binds the redirect to the CLI process that started the flow.
pub(crate) fn parse_callback(path: &str, expected_state: &str) -> Option<LoopbackOutcome> {
    let query = path.strip_prefix("/cb?")?;
    let mut state = None;
    let mut outcome = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("state", v)) => state = Some(v),
            Some(("outcome", v)) => outcome = Some(v),
            _ => {}
        }
    }
    if state != Some(expected_state) || expected_state.is_empty() {
        return None;
    }
    match outcome {
        Some("approved") => Some(LoopbackOutcome::Approved),
        Some("denied") => Some(LoopbackOutcome::Denied),
        _ => None,
    }
}

/// The full auto-open URL: the pending disclosure's approval page + the flow challenge (when the
/// page doesn't already carry one — an invitation enrollment's does) + this listener's return
/// coordinates. Everything appended is non-secret.
pub(crate) fn approval_url(base: &str, challenge: &str, port: u16, state: &str) -> String {
    let mut url = base.to_owned();
    let mut sep = if url.contains('?') { '&' } else { '?' };
    if !url.contains("device=") {
        url.push(sep);
        url.push_str("device=");
        url.push_str(challenge);
        sep = '&';
    }
    url.push(sep);
    url.push_str(&format!("port={port}&state={state}"));
    url
}

/// The environment facts the browser-open decision reads — a snapshot, so the decision itself
/// is a pure function ([`choose_browser`]) the tests table-drive.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BrowserEnv {
    /// A human is watching (TTY stderr, not `--json`).
    pub interactive: bool,
    /// STDOUT is a terminal — the wait may block indefinitely (a piped run without an explicit
    /// `--wait` exits after one poll, so a loopback ceremony there would be orphaned).
    pub stdout_tty: bool,
    /// `--wait` was given in any form — the caller explicitly opted into a blocking wait, so the
    /// process stays alive to receive the redirect even when piped.
    pub explicit_wait: bool,
    /// An SSH session (`SSH_CONNECTION`/`SSH_TTY`) — the browser would open on the wrong machine.
    pub ssh: bool,
    /// `TOPOS_NO_BROWSER` set — the explicit opt-out.
    pub suppressed: bool,
    /// macOS (`open` always exists).
    pub macos: bool,
    /// A graphical session on other unixes (`DISPLAY`/`WAYLAND_DISPLAY`).
    pub display: bool,
}

impl BrowserEnv {
    /// Snapshot the real process environment (`interactive` / `stdout_tty` / `explicit_wait` are
    /// the caller's facts).
    pub(crate) fn detect(interactive: bool, stdout_tty: bool, explicit_wait: bool) -> Self {
        let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        Self {
            interactive,
            stdout_tty,
            explicit_wait,
            ssh: set("SSH_CONNECTION") || set("SSH_TTY"),
            suppressed: set("TOPOS_NO_BROWSER"),
            macos: cfg!(target_os = "macos"),
            display: set("DISPLAY") || set("WAYLAND_DISPLAY"),
        }
    }
}

/// Choose the browser opener, or `None` for the typed-code fallback. Pure and table-tested:
/// headless / `--json` / SSH / opted-out never open anything, and a PIPED run without an explicit
/// `--wait` never does either — the loopback ceremony needs the process to outlive the human's
/// click, and the piped default exits after one poll. macOS uses `open`; a graphical unix session
/// uses `xdg-open`; anything else falls back.
pub(crate) fn choose_browser(env: &BrowserEnv) -> Option<&'static str> {
    if !env.interactive || env.ssh || env.suppressed {
        return None;
    }
    if !env.stdout_tty && !env.explicit_wait {
        return None;
    }
    if env.macos {
        return Some("open");
    }
    if env.display {
        return Some("xdg-open");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(
        interactive: bool,
        stdout_tty: bool,
        explicit_wait: bool,
        ssh: bool,
        suppressed: bool,
        macos: bool,
        display: bool,
    ) -> BrowserEnv {
        BrowserEnv {
            interactive,
            stdout_tty,
            explicit_wait,
            ssh,
            suppressed,
            macos,
            display,
        }
    }

    #[test]
    fn browser_selection_is_a_pure_table() {
        // A full TTY on macOS / a graphical unix opens; SSH / suppressed / headless never do.
        assert_eq!(
            choose_browser(&env(true, true, false, false, false, true, false)),
            Some("open")
        );
        assert_eq!(
            choose_browser(&env(true, true, false, false, false, false, true)),
            Some("xdg-open")
        );
        assert_eq!(
            choose_browser(&env(true, true, false, false, false, false, false)),
            None
        );
        assert_eq!(
            choose_browser(&env(false, true, false, false, false, true, true)),
            None
        );
        assert_eq!(
            choose_browser(&env(true, true, false, true, false, true, true)),
            None
        );
        assert_eq!(
            choose_browser(&env(true, true, false, false, true, true, true)),
            None
        );
    }

    #[test]
    fn a_piped_run_opens_no_browser_unless_wait_was_explicit() {
        // Piped stdout, no --wait: the wait exits after one poll, so a loopback ceremony would be
        // orphaned — never open. An explicit --wait keeps the process alive → the loopback may run.
        assert_eq!(
            choose_browser(&env(true, false, false, false, false, true, true)),
            None
        );
        assert_eq!(
            choose_browser(&env(true, false, true, false, false, true, false)),
            Some("open")
        );
        assert_eq!(
            choose_browser(&env(true, false, true, false, false, false, true)),
            Some("xdg-open")
        );
        // --wait does not override the harder refusals (SSH / opt-out / non-interactive).
        assert_eq!(
            choose_browser(&env(true, false, true, true, false, true, true)),
            None
        );
        assert_eq!(
            choose_browser(&env(false, false, true, false, false, true, true)),
            None
        );
    }

    #[test]
    fn callback_parsing_binds_to_the_exact_state() {
        let s = "st-0123456789";
        assert_eq!(
            parse_callback("/cb?state=st-0123456789&outcome=approved", s),
            Some(LoopbackOutcome::Approved)
        );
        assert_eq!(
            parse_callback("/cb?outcome=denied&state=st-0123456789", s),
            Some(LoopbackOutcome::Denied)
        );
        // Wrong / missing state, wrong path, unknown outcome: all refused.
        assert_eq!(parse_callback("/cb?state=other&outcome=approved", s), None);
        assert_eq!(parse_callback("/cb?outcome=approved", s), None);
        assert_eq!(parse_callback("/favicon.ico", s), None);
        assert_eq!(
            parse_callback("/cb?state=st-0123456789&outcome=maybe", s),
            None
        );
        // An empty expected state can never match (fail-closed).
        assert_eq!(parse_callback("/cb?state=&outcome=approved", ""), None);
    }

    #[test]
    fn approval_url_appends_challenge_once_and_the_return_coordinates() {
        // A plain /verify page gains the challenge + coordinates.
        assert_eq!(
            approval_url("https://x/verify", "ab12", 4321, "st"),
            "https://x/verify?device=ab12&port=4321&state=st"
        );
        // An invitation page already carrying its challenge gains only the coordinates.
        assert_eq!(
            approval_url("https://x/invite/tok?device=ab12", "ab12", 4321, "st"),
            "https://x/invite/tok?device=ab12&port=4321&state=st"
        );
    }

    #[test]
    fn listener_receives_exactly_the_state_bound_redirect() {
        let listener = LoopbackListener::bind("st-abcdef".to_owned()).expect("bind loopback");
        let port = listener.port();
        assert_eq!(listener.try_receive(), None, "nothing arrived yet");

        // A wrong-state probe answers 404 and spends nothing. The single test thread writes
        // FIRST, lets try_receive accept+answer, then reads the buffered response.
        let mut probe = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        probe
            .write_all(b"GET /cb?state=wrong&outcome=approved HTTP/1.1\r\nhost: x\r\n\r\n")
            .expect("write");
        assert_eq!(listener.try_receive(), None);
        let mut answer = String::new();
        let _ = probe.read_to_string(&mut answer);
        assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");

        // The ceremony's own redirect lands.
        let mut real = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        real.write_all(b"GET /cb?state=st-abcdef&outcome=approved HTTP/1.1\r\nhost: x\r\n\r\n")
            .expect("write");
        assert_eq!(listener.try_receive(), Some(LoopbackOutcome::Approved));
        let mut answer = String::new();
        let _ = real.read_to_string(&mut answer);
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    }
}
