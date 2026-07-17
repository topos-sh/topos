//! `codex` — the session-start auto-update hook in `<root>/config.toml` (production root:
//! `$CODEX_HOME` else `~/.codex`). The config is TOML and no TOML dependency exists in this
//! crate, so the edit mirrors the Hermes YAML discipline: a **line-anchored merge** handling
//! ONLY provable shapes, byte-preserving everywhere else, failing closed (`Degraded`, ZERO
//! writes) on anything it cannot prove.
//!
//! Install appends, at EOF, one sentinel-marked block:
//!
//! ```toml
//! # topos:currency
//! [[hooks.SessionStart]]
//! [[hooks.SessionStart.hooks]]
//! type = "command"
//! command = "command -v topos >/dev/null 2>&1 && topos update --quiet || true"
//! ```
//!
//! — and ONLY when the existing file holds no hooks table (no line whose trim starts with
//! `[hooks` or `[[hooks`, and no top-level-looking `hooks =` assignment, which an appended block
//! would duplicate) and no sentinel already. A present sentinel with our exact contiguous block
//! is a true no-op; a sentinel in any other shape, or any existing hooks table, fails closed /
//! adopts-or-leaves. Remove scrubs exactly the contiguous block it wrote — anchored on the
//! sentinel comment line through the block's last `command = …` line and byte-verified against
//! the canonical block before deleting — otherwise `Degraded`, zero writes.
//!
//! **Evidence level:** the hook configuration struct names were verified against a live
//! codex-cli 0.144.4 binary (2026-07-16); the exact TOML nesting above is INFERRED from those
//! structs, not probed end-to-end.
//!
//! **Consent posture — `Active` is NEVER claimed:** codex gates hooks behind persisted
//! per-definition trust granted in its own UI, and that trust store is not readable evidence.
//! A successful install reports `Inactive` + the explicit-pull floor with a note naming the
//! step still owed; the kind when the hook would fire is `SessionStart`. This adapter never
//! writes codex's trust state.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, TriggerState};

use crate::ConfigStore;

use super::{GUARDED_SWEEP, SENTINEL, TriggerAdapter, TriggerOutcome, env_override, outcome};

/// Codex's user config file, under the resolved root.
const CONFIG_FILENAME: &str = "config.toml";

/// The structured marker identity reported in [`TriggerOutcome::marker_id`].
const MARKER_ID: &str = "topos:codex:currency:1";

/// The consent step still owed after a successful registration (codex's own trust prompt).
const NOTE: &str =
    "trust the hook inside Codex (it will prompt) — until then, explicit `topos update`";

/// The exact block install appends (also the whole fresh-file config): the sentinel anchor line,
/// the two array-of-tables headers, and the guarded sweep — quoted, WITHOUT the in-command
/// sentinel suffix (the anchor line is the sentinel here; TOML has no inert in-string comment).
/// Composed from the shared consts so the one sweep spelling can never drift per-surface.
fn block() -> String {
    format!(
        "{SENTINEL}\n[[hooks.SessionStart]]\n[[hooks.SessionStart.hooks]]\ntype = \"command\"\ncommand = \"{GUARDED_SWEEP}\"\n"
    )
}

/// The `codex` [`TriggerAdapter`]. Holds the resolved config root (injected, so tests never
/// touch a real `~/.codex`) and the [`ConfigStore`] port.
pub(crate) struct Codex<'a> {
    root: PathBuf,
    cfg: &'a dyn ConfigStore,
}

/// Production root: `$CODEX_HOME` (codex's own override, resolved the way the registry does)
/// else `~/.codex` under the passed home.
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    env_override("CODEX_HOME").unwrap_or_else(|| home.join(".codex"))
}

pub(crate) fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> Codex<'a> {
    Codex::new(resolve_root(home), cfg)
}

impl<'a> Codex<'a> {
    pub(crate) fn new(root: PathBuf, cfg: &'a dyn ConfigStore) -> Self {
        Self { root, cfg }
    }

    fn config_path(&self) -> PathBuf {
        self.root.join(CONFIG_FILENAME)
    }

    fn out(
        &self,
        state: TriggerState,
        touched: bool,
        note: Option<&'static str>,
    ) -> TriggerOutcome {
        outcome(
            "codex",
            CurrencyKind::SessionStart, // what fires when live; never reported live (see above)
            state,
            touched.then(|| self.config_path().to_string_lossy().into_owned()),
            MARKER_ID,
            note,
        )
    }

    fn apply(&self, plan: EditPlan) -> TriggerOutcome {
        match plan {
            EditPlan::Leave(state, note) => self.out(state, false, note),
            EditPlan::Write(bytes, state, note) => {
                match self.cfg.replace(&self.config_path(), &bytes) {
                    Ok(()) => self.out(state, true, note),
                    Err(_) => self.out(TriggerState::Degraded, false, None),
                }
            }
        }
    }
}

impl TriggerAdapter for Codex<'_> {
    fn slug(&self) -> &'static str {
        "codex"
    }

    fn install(&self) -> TriggerOutcome {
        match self.cfg.read(&self.config_path()) {
            Ok(current) => self.apply(plan_install(current.as_deref())),
            Err(_) => self.out(TriggerState::Degraded, false, None),
        }
    }

    fn remove(&self) -> TriggerOutcome {
        match self.cfg.read(&self.config_path()) {
            Ok(current) => self.apply(plan_remove(current.as_deref())),
            Err(_) => self.out(TriggerState::Degraded, false, None),
        }
    }

    /// Presence = the sentinel anchor with our byte-verified block, right now. Anything
    /// unreadable or tampered answers `false`.
    fn present(&self) -> bool {
        let Ok(Some(bytes)) = self.cfg.read(&self.config_path()) else {
            return false;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return false;
        };
        let lines = split_lines(text);
        matches!(sentinel_indices(&lines)[..], [i] if block_verified(&lines, i))
    }
}

// ---------------------------------------------------------------------------------------------
// The pure line-anchored merge — bytes in → an edit plan out. No I/O; no TOML parse: exactly the
// shapes provable by line anchors, everything else fails closed.
// ---------------------------------------------------------------------------------------------

enum EditPlan {
    Write(Vec<u8>, TriggerState, Option<&'static str>),
    Leave(TriggerState, Option<&'static str>),
}

/// Split preserving each line's bytes (terminators included), so untouched lines re-emit
/// verbatim and the block verify is byte-exact.
fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn sentinel_indices(lines: &[&str]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == SENTINEL)
        .map(|(i, _)| i)
        .collect()
}

/// Whether the lines from `start` are byte-for-byte our canonical block.
fn block_verified(lines: &[&str], start: usize) -> bool {
    let block = block();
    let n = block.split_inclusive('\n').count();
    start + n <= lines.len() && lines[start..start + n].concat() == block
}

/// A line that puts a `hooks` table (or array-of-tables) in play — appending our block beside it
/// is never provable. The prefix match is deliberately broad (`[hooks…` of any spelling): fail
/// closed beats guessing.
fn is_hooks_table_line(trimmed: &str) -> bool {
    trimmed.starts_with("[hooks") || trimmed.starts_with("[[hooks")
}

/// A `hooks = …` assignment (or a quoted `"hooks"` key) — an appended `[[hooks.…]]` header would
/// duplicate the key and break codex's parse, so this too is never provable.
fn is_hooks_key_line(trimmed: &str) -> bool {
    trimmed.starts_with("\"hooks\"")
        || trimmed.starts_with("'hooks'")
        || trimmed
            .strip_prefix("hooks")
            .is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// Whether any line puts a hooks surface in play (outside our own verified block, which callers
/// rule out first by the sentinel check).
fn has_foreign_hooks(lines: &[&str]) -> bool {
    lines.iter().any(|l| {
        let t = l.trim();
        is_hooks_table_line(t) || is_hooks_key_line(t)
    })
}

/// A hand-rolled topos hook somewhere in that foreign hooks surface — adopt-or-leave rather than
/// a plain degrade.
fn mentions_topos_command(text: &str) -> bool {
    text.contains("topos update") || text.contains("topos pull")
}

fn plan_install(current: Option<&[u8]>) -> EditPlan {
    let placed = TriggerState::Inactive; // NEVER Active: codex's trust prompt is still owed
    let text = match current {
        None => return EditPlan::Write(block().into_bytes(), placed, Some(NOTE)),
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(t) if t.trim().is_empty() => {
                return EditPlan::Write(block().into_bytes(), placed, Some(NOTE));
            }
            Ok(t) => t,
            Err(_) => return EditPlan::Leave(TriggerState::Degraded, None),
        },
    };
    // A byte-order mark hides the first line's true content from the line anchors — never
    // reasoned about.
    if text.starts_with('\u{feff}') {
        return EditPlan::Leave(TriggerState::Degraded, None);
    }
    let lines = split_lines(text);
    match sentinel_indices(&lines)[..] {
        [] => {}
        // Sentinel present with our exact block → a true no-op (still honestly not live).
        [i] if block_verified(&lines, i) => return EditPlan::Leave(placed, Some(NOTE)),
        // Sentinel in any other shape (tampered block, duplicates) → never touched.
        _ => return EditPlan::Leave(TriggerState::Degraded, None),
    }
    if has_foreign_hooks(&lines) {
        // An existing hooks surface we did not write: a hand-rolled topos hook is
        // adopt-or-leave; anything else is unprovable — either way, zero writes.
        return if mentions_topos_command(text) {
            EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged, None)
        } else {
            EditPlan::Leave(TriggerState::Degraded, None)
        };
    }
    // Provably clean: append at EOF (terminating an unterminated last line first — the
    // `[[hooks.…]]` headers are absolute paths, so an EOF append is always top-level).
    let mut out = text.as_bytes().to_vec();
    if !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(block().as_bytes());
    EditPlan::Write(out, placed, Some(NOTE))
}

fn plan_remove(current: Option<&[u8]>) -> EditPlan {
    let text = match current {
        None => return EditPlan::Leave(TriggerState::Inactive, None), // nothing to remove
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(t) => t,
            Err(_) => return EditPlan::Leave(TriggerState::Degraded, None),
        },
    };
    let lines = split_lines(text);
    match sentinel_indices(&lines)[..] {
        [] => {
            if has_foreign_hooks(&lines) && mentions_topos_command(text) {
                EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged, None)
            } else {
                EditPlan::Leave(TriggerState::Inactive, None)
            }
        }
        [i] if block_verified(&lines, i) => {
            // Scrub exactly the contiguous block we wrote — every other byte verbatim.
            let n = block().split_inclusive('\n').count();
            let mut out = String::with_capacity(text.len());
            for (idx, line) in lines.iter().enumerate() {
                if (i..i + n).contains(&idx) {
                    continue;
                }
                out.push_str(line);
            }
            EditPlan::Write(out.into_bytes(), TriggerState::Inactive, None)
        }
        // The sentinel is there but the block does not byte-verify (a hand edit): deleting on a
        // guess could take user bytes with it — Degraded, zero writes.
        _ => EditPlan::Leave(TriggerState::Degraded, None),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{ErrConfig, MemConfig};
    use super::*;

    fn a<'c>(cfg: &'c MemConfig) -> Codex<'c> {
        Codex::new(PathBuf::from("/x"), cfg)
    }

    const CONFIG: &str = "/x/config.toml";

    /// The byte-exact block fixture — what a fresh install writes, pinned as a literal so a
    /// drift in the composed `block()` (or the shared consts) fails loudly here.
    const BLOCK_FIXTURE: &str = r#"# topos:currency
[[hooks.SessionStart]]
[[hooks.SessionStart.hooks]]
type = "command"
command = "command -v topos >/dev/null 2>&1 && topos update --quiet || true"
"#;

    #[test]
    fn fresh_install_writes_exactly_the_block_and_never_claims_active() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.slug, "codex");
        assert_eq!(report.marker_id, MARKER_ID);
        // Codex gates hooks behind its own trust prompt — Active is never claimed.
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.kind, CurrencyKind::ExplicitPullOnly);
        assert!(report.note.as_deref().unwrap().contains("trust the hook"));
        assert_eq!(report.touched_path.as_deref(), Some(CONFIG));
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(BLOCK_FIXTURE));
        assert_eq!(cfg.writes(), 1);
        // The block's command is the guarded sweep, quoted, without an in-string sentinel.
        assert!(block().contains(&format!("command = \"{GUARDED_SWEEP}\"")));
    }

    #[test]
    fn install_appends_at_eof_and_terminates_an_unterminated_last_line() {
        let cfg = MemConfig::with_file(CONFIG, "model = \"gpt-5-codex\""); // no trailing newline
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(
            cfg.text(CONFIG),
            Some(format!("model = \"gpt-5-codex\"\n{BLOCK_FIXTURE}")),
        );
    }

    #[test]
    fn install_is_idempotent_a_true_no_op_on_rerun() {
        let cfg = MemConfig::with_file(CONFIG, "model = \"gpt-5-codex\"\n");
        a(&cfg).install();
        let after_first = cfg.text(CONFIG);
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(
            report.note.is_some(),
            "the consent note rides the no-op too"
        );
        assert!(report.touched_path.is_none());
        assert_eq!(cfg.writes(), 1, "second install writes nothing");
        assert_eq!(cfg.text(CONFIG), after_first);
    }

    #[test]
    fn any_existing_hooks_surface_fails_closed_or_adopts() {
        // A foreign hooks table with no topos command → unprovable, zero writes.
        for shape in [
            "[hooks]\nfoo = 1\n",
            "[[hooks.SessionStart]]\ncommand = \"echo hi\"\n",
            "hooks = { }\n",
            "\"hooks\" = { }\n",
        ] {
            let cfg = MemConfig::with_file(CONFIG, shape);
            let report = a(&cfg).install();
            assert_eq!(report.state, TriggerState::Degraded, "{shape:?}");
            assert_eq!(cfg.writes(), 0);
            assert_eq!(cfg.text(CONFIG).as_deref(), Some(shape), "byte-untouched");
        }
        // A hand-rolled topos hook inside a hooks table → adopt-or-leave.
        let cfg = MemConfig::with_file(
            CONFIG,
            "[[hooks.SessionStart]]\ncommand = \"topos update --quiet\"\n",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0);
        assert_eq!(
            a(&cfg).remove().state,
            TriggerState::AlreadyPresentUnmanaged
        );
    }

    #[test]
    fn a_tampered_sentinel_block_degrades_with_zero_writes() {
        // The sentinel line survives but the block was hand-edited: neither install nor remove
        // will guess at a deletion.
        let tampered = "# topos:currency\n[[hooks.SessionStart]]\ncommand = \"something else\"\n";
        let cfg = MemConfig::with_file(CONFIG, tampered);
        assert_eq!(a(&cfg).install().state, TriggerState::Degraded);
        assert_eq!(a(&cfg).remove().state, TriggerState::Degraded);
        assert_eq!(cfg.writes(), 0);
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(tampered));
        assert!(
            !a(&cfg).present(),
            "a tampered block is not provable presence"
        );
    }

    #[test]
    fn non_utf8_bytes_degrade_with_zero_writes() {
        let cfg = MemConfig::default();
        cfg.set_raw(CONFIG, b"\xff\xfe not text");
        assert_eq!(a(&cfg).install().state, TriggerState::Degraded);
        assert_eq!(a(&cfg).remove().state, TriggerState::Degraded);
        assert_eq!(cfg.writes(), 0);
    }

    #[test]
    fn remove_scrubs_exactly_the_block_then_is_idempotent() {
        let before = "model = \"gpt-5-codex\"\nsandbox = \"strict\"\n";
        let cfg = MemConfig::with_file(CONFIG, before);
        a(&cfg).install();

        let report = a(&cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.kind, CurrencyKind::ExplicitPullOnly);
        assert_eq!(
            cfg.text(CONFIG).as_deref(),
            Some(before),
            "the user's bytes are restored exactly"
        );

        let writes = cfg.writes();
        let again = a(&cfg).remove();
        assert_eq!(again.state, TriggerState::Inactive);
        assert_eq!(cfg.writes(), writes, "second remove writes nothing");

        // A remove over an absent file is a clean no-op too.
        let absent = MemConfig::default();
        assert_eq!(a(&absent).remove().state, TriggerState::Inactive);
        assert!(
            absent.text(CONFIG).is_none(),
            "remove never creates the file"
        );
    }

    #[test]
    fn present_is_honest() {
        let cfg = MemConfig::default();
        let adapter = a(&cfg);
        assert!(!adapter.present());
        adapter.install();
        assert!(
            adapter.present(),
            "the artifact is present even though the trust step is still owed"
        );
        adapter.remove();
        assert!(!adapter.present());
    }

    #[test]
    fn an_unreadable_store_degrades_with_zero_writes() {
        let cfg = ErrConfig;
        let adapter = Codex::new(PathBuf::from("/x"), &cfg);
        assert_eq!(adapter.install().state, TriggerState::Degraded);
        assert_eq!(adapter.remove().state, TriggerState::Degraded);
        assert!(!adapter.present());
    }
}
