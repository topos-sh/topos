//! The process's OWN two streams. Every line the program prints goes through here.
//!
//! `println!` panics when its reader is gone (`topos list | head` closes the pipe as soon as it has
//! its lines), and the classic cure — restoring the default `SIGPIPE` disposition — needs `unsafe`,
//! which this workspace forbids. So a closed pipe is handled in Rust instead: the write is dropped
//! and the stream remembers it.
//!
//! THE TWO STREAMS ARE INDEPENDENT, and the difference decides an exit code. A gone STDOUT reader
//! has everything it asked for, so the run ends cleanly ([`stdout_closed`] is what
//! [`crate::app::run`] reads). A gone STDERR reader has only stopped listening to diagnostics:
//! it silences further stderr lines and NOTHING else — not the exit code, not the outcome still
//! being written to stdout. One shared latch would have turned `topos add … 2> >(head)` into a
//! success and swallowed the answer.
//!
//! Any OTHER write failure keeps `println!`'s own behavior — a full disk must stay loud, never a
//! silent success.

use std::fmt::Arguments;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// One output stream's own state. Process-wide because the condition is: the pipe belongs to the
/// process, and nothing that follows can un-close it.
struct Stream {
    /// Latched by the first write that finds no reader.
    closed: AtomicBool,
    /// How a panic names this stream, matching the `std` print macros' wording.
    name: &'static str,
}

impl Stream {
    const fn new(name: &'static str) -> Self {
        Self {
            closed: AtomicBool::new(false),
            name,
        }
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Write one line. A broken pipe latches THIS stream and returns; every other error panics
    /// exactly as the `std` print macros do, so a write that really failed is never mistaken for
    /// one nobody was listening to.
    fn write_line(&self, w: &mut impl Write, args: Arguments<'_>) {
        if self.closed() {
            return;
        }
        match writeln!(w, "{args}") {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                self.closed.store(true, Ordering::Relaxed);
            }
            Err(e) => panic!("failed printing to {}: {e}", self.name),
        }
    }
}

static STDOUT: Stream = Stream::new("stdout");
static STDERR: Stream = Stream::new("stderr");

/// Whether a stdout write has met a closed pipe — the ONE stream whose reader leaving ends the run.
/// The composition root turns this into the exit code.
pub(crate) fn stdout_closed() -> bool {
    STDOUT.closed()
}

/// One stdout line — the outcome channel (`--json` envelopes, receipts, TTY renders).
pub(crate) fn stdout_line(args: Arguments<'_>) {
    STDOUT.write_line(&mut io::stdout().lock(), args);
}

/// One stdout PROTOCOL line — the relay's channel ([`crate::ops::relay`]): written and FLUSHED in
/// one lock, because a protocol peer blocks on the reply and a piped stdout is block-buffered —
/// an unflushed line would sit in the buffer exactly as long as the peer waits for it. The flush
/// meets the same latch discipline as the write: a broken pipe latches the stream, anything else
/// panics as the `std` macros would.
pub(crate) fn stdout_protocol_line(args: Arguments<'_>) {
    if STDOUT.closed() {
        return;
    }
    let mut w = io::stdout().lock();
    STDOUT.write_line(&mut w, args);
    if STDOUT.closed() {
        return;
    }
    match w.flush() {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
            STDOUT.closed.store(true, Ordering::Relaxed);
        }
        Err(e) => panic!("failed printing to {}: {e}", STDOUT.name),
    }
}

/// One stderr line — the diagnostics channel (warnings, refusals, the version nag).
pub(crate) fn stderr_line(args: Arguments<'_>) {
    STDERR.write_line(&mut io::stderr().lock(), args);
}

/// `println!` that survives a closed pipe — see the module docs.
macro_rules! outln {
    ($($arg:tt)*) => { $crate::out::stdout_line(format_args!($($arg)*)) };
}

/// `eprintln!` that survives a closed pipe — see the module docs.
macro_rules! errln {
    ($($arg:tt)*) => { $crate::out::stderr_line(format_args!($($arg)*)) };
}

pub(crate) use {errln, outln};

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink whose every write reports the reader as gone — the `| head` shape, in-process.
    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A sink that records what reached it.
    #[derive(Default)]
    struct Sink(Vec<u8>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_reader_latches_its_own_stream_and_only_its_own() {
        // The latches are process-wide (the pipes are the process's), so this test sets its own
        // start state rather than assuming one, and puts it back at the end.
        STDOUT.closed.store(false, Ordering::Relaxed);
        STDERR.closed.store(false, Ordering::Relaxed);

        // A gone STDERR reader silences stderr — and touches nothing else. Were the state shared,
        // the exit code below would flip and the outcome after it would vanish.
        STDERR.write_line(&mut Closed, format_args!("a diagnostic nobody reads"));
        assert!(STDERR.closed());
        assert!(!stdout_closed(), "stderr's reader is not stdout's");
        let mut still_listening = Sink::default();
        STDOUT.write_line(&mut still_listening, format_args!("the answer"));
        assert_eq!(still_listening.0, b"the answer\n");

        // A gone STDOUT reader latches stdout, and every later write is a silent no-op — never a
        // second panic-or-error decision.
        STDOUT.write_line(&mut Closed, format_args!("a line nobody reads"));
        assert!(stdout_closed(), "the closed pipe is remembered");
        STDOUT.write_line(&mut Closed, format_args!("and another"));

        STDOUT.closed.store(false, Ordering::Relaxed);
        STDERR.closed.store(false, Ordering::Relaxed);
    }
}
