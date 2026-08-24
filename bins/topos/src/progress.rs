//! The ACTIVITY seam — how a blocking verb says "still working" while a fetch is in flight.
//!
//! Every verb still produces exactly ONE typed outcome, rendered twice (the `--json` envelope or the
//! TTY receipt) on **stdout**. This module owns a second, unschema'd channel on **stderr**: a
//! transient line naming the activity in flight, so a minute-long forge import or bundle download is
//! not a silent terminal. Nothing here is ever part of an outcome, and nothing here may ever reach
//! stdout — the golden `--json` fixtures assert stdout byte-for-byte, and stderr is outside that
//! contract (the same place the recovery disclosures and the version nag already speak).
//!
//! ## The shape of a phase
//!
//! A **phase** is one named unit of activity ("fetching github.com/o/r", "updating docs (2 of 3)").
//! At most one is in flight at a time, and whoever opened it closes it — [`phase`] hands back a
//! guard that ends it on drop, so an early return can never strand a live line. Byte progress
//! ([`ProgressSink::bytes`]) always belongs to whatever phase is in flight, which is what lets the
//! transports report bytes for a phase a verb named: the op says WHAT is being fetched, the
//! transport says HOW FAR along it is.
//!
//! Nesting is deliberately not a thing. A verb-level label ("updating docs") is more useful than the
//! transport-level one underneath it ("contacting topos.sh"), so the inner layers open theirs with
//! [`phase_if_idle`] — which does nothing when something more specific already holds the line.
//!
//! ## The three sinks
//!
//! - [`Tty`] — stderr is a terminal: ONE repainted line (an `indicatif` spinner + the label + human
//!   bytes), steady-ticked so it stays alive through a stalled socket, and CLEARED on end so the
//!   receipt that follows on stdout prints against a clean screen.
//! - [`Plain`] — stderr is a pipe or a file: one plain `topos: <label>` line the first time a phase
//!   opens (the same prefix the recovery disclosures use), present-participle, no repainting and no
//!   spinner glyphs — a log reader gets a legible trace instead of a stream of escape codes. A
//!   label already announced is never announced again, so a verb making four requests behind one
//!   label reads the way it does on a terminal, where one line is repainted.
//! - [`Silent`] — nothing at all. The harness hook sweep (`update --quiet`), and every unit test.

use std::cell::{Cell, RefCell};
use std::io::{self, IsTerminal, Write};
use std::rc::Rc;
use std::time::Duration;

use indicatif::{DecimalBytes, ProgressBar};

/// How often the animated sink repaints its line while nothing else happens — fast enough to read
/// as alive, slow enough to cost nothing. (`indicatif` rate-limits the actual terminal writes on top
/// of this.)
const TICK: Duration = Duration::from_millis(100);

/// The one channel a long-running verb reports activity through. Implemented three ways ([`Tty`],
/// [`Plain`], [`Silent`]); injected through [`Ctx`](crate::ctx::Ctx) for the ops and cloned into the
/// transports, exactly like the fs/clock/id seams — never reached through a global.
///
/// Callers should prefer the [`phase`] / [`phase_if_idle`] guards over calling `begin`/`end` in
/// pairs by hand; the raw methods exist so the guards (and the impls) have something to sit on.
pub(crate) trait ProgressSink {
    /// Open `label` as the phase in flight, ending whatever was there. `label` is user-facing text
    /// in the present participle ("fetching …", "downloading …") — no trailing punctuation, no
    /// internals (a URL path, an opaque id), and never the word the docs ban for auto-update.
    fn begin(&self, label: &str);

    /// Open `label` only when NOTHING is in flight. Answers whether THIS call opened it — the
    /// caller must not end a phase it did not open. This is how the lower layers (a transport
    /// naming the server it is dialing) stay quiet under a verb that already said something better.
    fn begin_if_idle(&self, label: &str) -> bool;

    /// Report byte progress within the phase in flight: `done` bytes so far, `total` when the
    /// server declared a length. No phase in flight ⇒ nothing to attach to ⇒ ignored.
    fn bytes(&self, done: u64, total: Option<u64>);

    /// End the phase in flight and clear its line. Idempotent — ending nothing is a no-op.
    fn end(&self);
}

/// The phase guard [`phase`] hands back — ends the phase it opened when it drops, so an early
/// return, a `?`, or a panic can never leave a live line behind.
pub(crate) struct Phase<'a> {
    /// `None` when this guard did not open the phase ([`phase_if_idle`] on a busy sink) — it then
    /// ends nothing, because the line belongs to someone else.
    sink: Option<&'a dyn ProgressSink>,
}

impl std::fmt::Debug for Phase<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Phase")
            .field("owns", &self.sink.is_some())
            .finish()
    }
}

impl Drop for Phase<'_> {
    fn drop(&mut self) {
        if let Some(sink) = self.sink {
            sink.end();
        }
    }
}

/// Open `label` as the phase in flight; the returned guard ends it on drop.
pub(crate) fn phase<'a>(sink: &'a dyn ProgressSink, label: &str) -> Phase<'a> {
    sink.begin(label);
    Phase { sink: Some(sink) }
}

/// Open `label` only if nothing is in flight (see [`ProgressSink::begin_if_idle`]). The guard ends
/// the phase only when this call is the one that opened it.
pub(crate) fn phase_if_idle<'a>(sink: &'a dyn ProgressSink, label: &str) -> Phase<'a> {
    Phase {
        sink: sink.begin_if_idle(label).then_some(sink),
    }
}

/// A guard over NOTHING — what a caller hands back when it decided it has nothing worth naming,
/// leaving the layers beneath it free to name their own. Ends nothing on drop.
pub(crate) fn no_phase<'a>() -> Phase<'a> {
    Phase { sink: None }
}

/// Which of the three sinks an invocation gets — the decision alone, so the rule is asserted
/// without a terminal to run against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SinkKind {
    Silent,
    Plain,
    Tty,
}

/// The selection rule. `quiet` is the harness hook sweep (`update --quiet`), which must stay
/// byte-silent on BOTH streams whatever the terminal looks like; otherwise stderr decides — a
/// terminal gets the animated line, a pipe or a file gets the plain one.
///
/// `--json` is deliberately NOT a factor. The envelope owns stdout and activity has never been part
/// of it, so an agent reading the document gets the same bytes either way while a human watching
/// the same run still sees something happen.
fn sink_kind(quiet: bool, stderr_is_terminal: bool) -> SinkKind {
    match (quiet, stderr_is_terminal) {
        (true, _) => SinkKind::Silent,
        (false, true) => SinkKind::Tty,
        (false, false) => SinkKind::Plain,
    }
}

/// Build the sink [`sink_kind`] picks for this invocation.
pub(crate) fn select(quiet: bool) -> Rc<dyn ProgressSink> {
    match sink_kind(quiet, io::stderr().is_terminal()) {
        SinkKind::Silent => Rc::new(Silent),
        SinkKind::Plain => Rc::new(Plain::new(io::stderr())),
        SinkKind::Tty => Rc::new(Tty::new()),
    }
}

/// The shared no-op sink — what the verbs that promise to dial nothing (`status`, the bare `topos`
/// orientation) and every test context carry. A `'static` reference, so it drops into a `Ctx`
/// literal anywhere without a binding to borrow from.
pub(crate) fn silent() -> &'static dyn ProgressSink {
    static SILENT: Silent = Silent;
    &SILENT
}

// =================================================================================================
// Silent — the no-op sink.
// =================================================================================================

/// Reports nothing, ever.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Silent;

impl ProgressSink for Silent {
    fn begin(&self, _label: &str) {}

    fn begin_if_idle(&self, _label: &str) -> bool {
        // Never claims a phase, so a guard over it never tries to end one.
        false
    }

    fn bytes(&self, _done: u64, _total: Option<u64>) {}

    fn end(&self) {}
}

// =================================================================================================
// Plain — one line per phase, for a stderr nobody is watching live.
// =================================================================================================

/// The line-per-phase sink: writes `topos: <label>` the first time that phase opens and says
/// nothing more. No cursor movement, no spinner glyph, no byte churn — a redirected stderr stays a
/// readable log. Generic over the writer so the shape is asserted in-crate against a buffer.
///
/// **One line per label per invocation.** The animated sink repaints ONE line, so a verb that makes
/// four requests behind one label looks identical whether it made one or four; here each request
/// used to cost another identical line, and `topos login --json` announced `contacting topos.sh`
/// twice for a single sign-in. A label already announced is never announced again — the reader
/// learned that fact the first time, and repeating it says nothing about progress.
#[derive(Debug)]
pub(crate) struct Plain<W: Write> {
    out: RefCell<W>,
    /// Whether a phase is in flight — what [`ProgressSink::begin_if_idle`] answers. Independent of
    /// whether the phase printed: a suppressed repeat still HOLDS the line, so the layers beneath
    /// it stay quiet exactly as they would under its first run.
    active: Cell<bool>,
    /// Every label this invocation has already announced.
    announced: RefCell<std::collections::BTreeSet<String>>,
}

impl<W: Write> Plain<W> {
    pub(crate) fn new(out: W) -> Self {
        Self {
            out: RefCell::new(out),
            active: Cell::new(false),
            announced: RefCell::new(std::collections::BTreeSet::new()),
        }
    }
}

impl<W: Write> ProgressSink for Plain<W> {
    fn begin(&self, label: &str) {
        self.active.set(true);
        if !self.announced.borrow_mut().insert(label.to_owned()) {
            return;
        }
        // Best-effort: a broken/full stderr must never fail a verb over a progress line.
        let mut out = self.out.borrow_mut();
        let _ = writeln!(out, "topos: {label}");
        let _ = out.flush();
    }

    fn begin_if_idle(&self, label: &str) -> bool {
        if self.active.get() {
            return false;
        }
        self.begin(label);
        true
    }

    fn bytes(&self, _done: u64, _total: Option<u64>) {
        // A line-per-phase sink cannot repaint, and one line per chunk would bury the log.
    }

    fn end(&self) {
        self.active.set(false);
    }
}

// =================================================================================================
// Tty — ONE repainted line on a terminal.
// =================================================================================================

/// The animated sink: one `indicatif` spinner on stderr, steady-ticked, carrying the phase label
/// and (once bytes start moving) how far along it is. Exactly ONE line exists at a time and it is
/// cleared when the phase ends, so the receipt printed on stdout afterwards is never interleaved
/// with a half-drawn bar.
pub(crate) struct Tty {
    /// The live line — `None` between phases.
    bar: RefCell<Option<ProgressBar>>,
    /// The phase label, kept so each byte report can re-render `<label> — <bytes>` without the
    /// caller having to repeat it.
    label: RefCell<String>,
}

impl std::fmt::Debug for Tty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tty")
            .field("in_flight", &self.bar.borrow().is_some())
            .finish_non_exhaustive()
    }
}

impl Tty {
    pub(crate) fn new() -> Self {
        Self {
            bar: RefCell::new(None),
            label: RefCell::new(String::new()),
        }
    }
}

impl Drop for Tty {
    /// The belt behind the phase guards: every phase is closed by the guard that opened it, but a
    /// line surviving into the receipt is the one failure worth being paranoid about, so the sink
    /// clears whatever is left when the invocation puts it down.
    fn drop(&mut self) {
        self.end();
    }
}

impl ProgressSink for Tty {
    fn begin(&self, label: &str) {
        self.end();
        // `new_spinner` already targets stderr with the default `{spinner} {msg}` style — and
        // renders as hidden when stderr is not a real terminal, which makes this safe even if the
        // selection above is ever wrong.
        let bar = ProgressBar::new_spinner();
        bar.set_message(label.to_owned());
        bar.enable_steady_tick(TICK);
        *self.label.borrow_mut() = label.to_owned();
        *self.bar.borrow_mut() = Some(bar);
    }

    fn begin_if_idle(&self, label: &str) -> bool {
        if self.bar.borrow().is_some() {
            return false;
        }
        self.begin(label);
        true
    }

    fn bytes(&self, done: u64, total: Option<u64>) {
        if let Some(bar) = self.bar.borrow().as_ref() {
            bar.set_message(byte_line(&self.label.borrow(), done, total));
        }
    }

    fn end(&self) {
        // Take it out FIRST: `finish_and_clear` stops the ticker thread, and the slot must already
        // read empty to anything that looks while we are here.
        let bar = self.bar.borrow_mut().take();
        if let Some(bar) = bar {
            bar.finish_and_clear();
        }
        self.label.borrow_mut().clear();
    }
}

/// The message a byte report renders: the phase label plus how far along it is — a percentage when
/// the server declared a length, otherwise just the running total. SI-prefixed (`12.4 MB`), which is
/// what a download's own progress everywhere else is quoted in. A zero-length declaration is treated
/// as no declaration: `0 of 0` is not progress, and dividing by it is worse.
fn byte_line(label: &str, done: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => {
            let pct = (done.min(total) * 100) / total;
            format!(
                "{label} — {pct}% ({} of {})",
                DecimalBytes(done),
                DecimalBytes(total)
            )
        }
        _ => format!("{label} — {}", DecimalBytes(done)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Plain, ProgressSink, Silent, SinkKind, byte_line, phase, phase_if_idle, silent, sink_kind,
    };

    #[test]
    fn the_hook_sweep_is_silent_whatever_the_terminal_and_stderr_decides_the_rest() {
        // The quiet sweep fires on every session-start-shaped event: byte-silent on both streams,
        // terminal or not.
        assert_eq!(sink_kind(true, true), SinkKind::Silent);
        assert_eq!(sink_kind(true, false), SinkKind::Silent);
        // Otherwise STDERR decides — never stdout, and never `--json` (the envelope owns stdout,
        // and this channel has never been part of it).
        assert_eq!(sink_kind(false, true), SinkKind::Tty);
        assert_eq!(sink_kind(false, false), SinkKind::Plain);
    }

    /// A `Plain` over an in-memory buffer, so the emitted shape is asserted byte-for-byte.
    fn captured() -> Plain<Vec<u8>> {
        Plain::new(Vec::new())
    }

    fn text(p: &Plain<Vec<u8>>) -> String {
        String::from_utf8(p.out.borrow().clone()).expect("progress lines are UTF-8")
    }

    #[test]
    fn plain_writes_one_prefixed_line_per_phase_and_nothing_for_bytes() {
        let p = captured();
        {
            let _phase = phase(&p, "fetching github.com/o/r");
            // Byte reports are invisible here — a line-per-phase sink cannot repaint, and one line
            // per chunk would bury the log it is meant to keep readable.
            p.bytes(1_500_000, Some(3_000_000));
            p.bytes(3_000_000, Some(3_000_000));
        }
        {
            let _phase = phase(&p, "downloading topos 0.1.18");
        }
        assert_eq!(
            text(&p),
            "topos: fetching github.com/o/r\ntopos: downloading topos 0.1.18\n"
        );
    }

    /// One invocation, one line per label. A verb that dials the same server four times used to
    /// cost four identical `topos: contacting topos.sh` lines — `topos login --json` announced it
    /// twice for a single sign-in — while a terminal repainted ONE line for the same work.
    #[test]
    fn a_label_is_announced_once_per_invocation_however_many_requests_it_covers() {
        let p = captured();
        for _ in 0..4 {
            let _phase = phase_if_idle(&p, "contacting topos.sh");
        }
        assert_eq!(text(&p), "topos: contacting topos.sh\n");
        // A DIFFERENT label is still its own line — the rule suppresses repetition, not activity.
        {
            let _phase = phase(&p, "downloading deploy-checklist");
        }
        // …and the first label, re-entered after it, stays said-once.
        {
            let _phase = phase_if_idle(&p, "contacting topos.sh");
        }
        assert_eq!(
            text(&p),
            "topos: contacting topos.sh\ntopos: downloading deploy-checklist\n"
        );
        // A suppressed repeat still HOLDS the line, so the layers beneath it stay quiet.
        let outer = phase(&p, "contacting topos.sh");
        assert!(p.active.get(), "a suppressed repeat still owns the phase");
        {
            let _inner = phase_if_idle(&p, "downloading deploy-checklist");
        }
        drop(outer);
        assert_eq!(
            text(&p),
            "topos: contacting topos.sh\ntopos: downloading deploy-checklist\n"
        );
    }

    #[test]
    fn the_inner_layers_stay_quiet_under_a_phase_the_verb_already_named() {
        let p = captured();
        let outer = phase(&p, "updating docs (2 of 3)");
        // The transport's fallback label loses to the verb's — and its guard ends nothing, so the
        // verb's phase survives the transport call that happened underneath it.
        {
            let _inner = phase_if_idle(&p, "contacting topos.sh");
        }
        assert!(
            p.active.get(),
            "the inner guard must not end the verb's phase"
        );
        // Nothing was opened by the inner call, so nothing was printed by it.
        assert_eq!(text(&p), "topos: updating docs (2 of 3)\n");
        // With the verb's phase over, the same fallback DOES get the line.
        drop(outer);
        let _inner = phase_if_idle(&p, "contacting topos.sh");
        assert_eq!(
            text(&p),
            "topos: updating docs (2 of 3)\ntopos: contacting topos.sh\n"
        );
    }

    #[test]
    fn silent_emits_nothing_and_never_claims_a_phase() {
        let s = Silent;
        s.begin("fetching github.com/o/r");
        s.bytes(10, Some(20));
        s.end();
        // The shared static answers identically — the two are the same behavior, one allocation
        // apart.
        assert!(!s.begin_if_idle("contacting topos.sh"));
        assert!(!silent().begin_if_idle("contacting topos.sh"));
        // A guard over a sink that claimed nothing is inert.
        let _phase = phase_if_idle(silent(), "downloading docs");
    }

    #[test]
    fn the_byte_line_shows_a_percentage_only_when_the_server_declared_a_length() {
        // Declared length → percent + both sides, SI-prefixed.
        assert_eq!(
            byte_line("downloading topos 0.1.18", 1_500_000, Some(3_000_000)),
            "downloading topos 0.1.18 — 50% (1.50 MB of 3.00 MB)"
        );
        // No length (a chunked forge tarball) → the running total alone, never a fabricated percent.
        assert_eq!(
            byte_line("fetching github.com/o/r", 12_400_000, None),
            "fetching github.com/o/r — 12.40 MB"
        );
        // A zero-length declaration is not a length: no percent, no division.
        assert_eq!(byte_line("x", 0, Some(0)), "x — 0 B");
        // A server that under-declares cannot push the percentage past 100.
        assert_eq!(byte_line("x", 30, Some(10)), "x — 100% (30 B of 10 B)");
    }
}
