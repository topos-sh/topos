//! `kimi-code-cli` — the session-start auto-update hook appended to `<root>/config.toml`
//! (production root: `~/.kimi-code`) as a sentinel-led `[[hooks]]` table:
//!
//! ```toml
//! # topos:currency
//! [[hooks]]
//! event = "SessionStart"
//! command = "command -v topos >/dev/null 2>&1 && topos update --quiet || true"
//! timeout = 60
//! ```
//!
//! Kimi keeps its hooks in the same `config.toml` that holds the rest of its settings, so this is a
//! surgical APPEND, not a file drop: the block is written at EOF, and every other line of a
//! person's config re-emits byte-for-byte. The edit is line-anchored in the Hermes/codex discipline
//! (no TOML dependency — see [`super::toml_lines`]): only shapes an anchor can prove are touched,
//! and anything else — non-UTF-8 bytes, a leading byte-order mark, a multiline string whose content
//! lines would read as structure, a `hooks` key already defined at the file's ROOT (where the
//! appended header would redefine it), a second sentinel line, a sentinel line that does not lead a
//! `[[hooks]]` table — fails closed with ZERO writes.
//!
//! **Ownership is the sentinel LINE**, which is also why the command here is the comment-less
//! guarded sweep: a TOML config has real comments, so the marker goes where a person reads it
//! rather than inside a quoted string. The block topos owns runs from that line to the end of the
//! table it leads (the next table header, or EOF) — that is the region a re-arm rewrites and a
//! scrub deletes, and nothing outside it is ever touched.
//!
//! **Evidence level: hook shape unverified** — the file, the sentinel-delimited append and the
//! `[[hooks]]` entry schema are taken from herdr's shipped Kimi Code integration (Apache-2.0),
//! which brackets its own block between two sentinel comments in exactly this file; no Kimi
//! documentation was read here and no live build was probed. Herdr registers twelve events to feed
//! an agent-state display; topos registers ONLY `SessionStart`, because what it needs is a refresh
//! moment. Nothing in that integration describes a per-hook consent gate, so a registration reports
//! `Active` carrying the evidence-level note.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, TriggerState};

use crate::{ConfigStore, trigger_report};

use super::toml_lines::{
    HeaderStated, basic_string_value, header_stated, is_key_line, root_defines_key,
    shape_is_unprovable, split_lines, table_header,
};
use super::{EditPlan, GUARDED_SWEEP, SENTINEL, TriggerAdapter, TriggerArtifact, TriggerReport};

const SLUG: &str = "kimi-code-cli";
const MARKER_ID: &str = "topos:kimi-code-cli:currency:1";
const NOTE: &str = "hook shape unverified";
/// The reason a config topos COULD read is still one it will not edit: `hooks` is already defined
/// at the file's top level (`hooks = []`, `hooks.enabled = false`), where the `[[hooks]]` header
/// this engine appends REDEFINES the name — which costs kimi its whole config rather than one
/// entry. The receipt names the file so a person can decide which shape they meant.
const ROOT_HOOKS_REASON: &str =
    "config.toml sets `hooks` at its top level, where topos cannot append `[[hooks]]`";
/// The header shape of the same refusal: a `[hooks]` table — or a dotted `[hooks.sub]` /
/// `[[hooks.sub]]`, which define the name implicitly as a plain table — already states what
/// `hooks` IS, and the `[[hooks]]` this engine appends would redefine it as an array. Only the
/// whole-array spelling leaves the append legal: one more `[[hooks]]` entry extends an array.
const HEADER_HOOKS_REASON: &str =
    "config.toml defines `hooks` in a shape topos cannot append a `[[hooks]]` entry to";
/// Kimi's user config — the hooks live in it beside the rest of its settings.
const CONFIG_FILENAME: &str = "config.toml";
/// The table header our block opens; also the only table a hand-rolled sweep is read out of.
const HOOKS_HEADER: &str = "[[hooks]]";

/// The canonical block, sentinel line first. Composed from the shared sweep const so the one
/// spelling can never drift per-surface (the byte-exact fixture is pinned in the tests); the sweep
/// is the COMMENT-LESS guarded form, because the ownership marker is the comment line above it.
fn block() -> String {
    format!(
        "{SENTINEL}\n{HOOKS_HEADER}\nevent = \"SessionStart\"\ncommand = \"{GUARDED_SWEEP}\"\ntimeout = 60\n"
    )
}

/// Production root: `~/.kimi-code` under the passed home (no env override in the registry table).
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    home.join(".kimi-code")
}

pub(crate) fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> KimiCodeCli<'a> {
    KimiCodeCli::new(resolve_root(home), cfg)
}

/// The `kimi-code-cli` [`TriggerAdapter`]: the sentinel-led TOML block over kimi's own config.
pub(crate) struct KimiCodeCli<'a> {
    root: PathBuf,
    cfg: &'a dyn ConfigStore,
}

impl<'a> KimiCodeCli<'a> {
    pub(crate) fn new(root: PathBuf, cfg: &'a dyn ConfigStore) -> Self {
        Self { root, cfg }
    }

    fn config_path(&self) -> PathBuf {
        self.root.join(CONFIG_FILENAME)
    }

    fn read(&self) -> std::io::Result<Option<Vec<u8>>> {
        self.cfg.read(&self.config_path())
    }

    fn out(&self, state: TriggerState, touched: bool, note: Option<&'static str>) -> TriggerReport {
        trigger_report(
            SLUG,
            CurrencyKind::SessionStart,
            state,
            touched.then(|| self.config_path().to_string_lossy().into_owned()),
            MARKER_ID,
            note,
        )
    }

    fn apply(&self, plan: EditPlan) -> TriggerReport {
        match plan {
            EditPlan::Leave(state, note) => self.out(state, false, note),
            EditPlan::Write(bytes, state, note) => {
                match self.cfg.replace(&self.config_path(), &bytes) {
                    Ok(()) => self.out(state, true, note),
                    Err(e) => self.out(TriggerState::Degraded, false, Some(crate::io_reason(&e))),
                }
            }
        }
    }
}

impl TriggerAdapter for KimiCodeCli<'_> {
    fn slug(&self) -> &'static str {
        SLUG
    }

    fn install(&self) -> TriggerReport {
        match self.read() {
            Ok(current) => self.apply(plan_install(current.as_deref())),
            // Unreadable (e.g. a permission error) — degrade honestly, never blind-overwrite.
            Err(e) => self.out(TriggerState::Degraded, false, Some(crate::io_reason(&e))),
        }
    }

    fn remove(&self) -> TriggerReport {
        match self.read() {
            Ok(current) => self.apply(plan_remove(current.as_deref())),
            Err(e) => self.out(TriggerState::Degraded, false, Some(crate::io_reason(&e))),
        }
    }

    /// Kimi's own config file — named whether or not our block is in it right now, because a file
    /// topos could not read is exactly the one it cannot prove anything about.
    fn config_file(&self) -> Option<PathBuf> {
        Some(self.config_path())
    }

    /// Presence = our sentinel-led block in a readable config, right now. A missing, unreadable, or
    /// unprovable file answers `false` — never a claim on faith.
    fn present(&self) -> bool {
        let Ok(Some(bytes)) = self.read() else {
            return false;
        };
        let Text::Provable(text) = text_of(Some(&bytes)) else {
            return false;
        };
        matches!(locate(&split_lines(text)), Located::Managed(..))
    }

    /// Kimi's config, disclosed ONLY while it actually holds our block — and never as a path
    /// `uninstall` deletes (the scrub takes the block, the file stays).
    fn artifacts(&self) -> Vec<TriggerArtifact> {
        if self.present() {
            vec![TriggerArtifact::Path(self.config_path())]
        } else {
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The pure line-anchored TOML merge — bytes in → an edit plan out. No I/O; no TOML parse: exactly
// the shapes provable by line anchors, everything else fails closed.
// ---------------------------------------------------------------------------------------------

/// The config bytes as something the anchors can stand on.
enum Text<'a> {
    /// Absent or whitespace-only — the block IS the file.
    Fresh,
    Provable(&'a str),
    /// Non-UTF-8, a byte-order mark hiding the first line's true content, or a shape whose lines
    /// are not all structure: a multiline string (whose `[[hooks]]` content line would read as a
    /// real table) or a key only TOML's escape rules could spell out.
    Unprovable,
}

fn text_of(current: Option<&[u8]>) -> Text<'_> {
    match current {
        None => Text::Fresh,
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(t) if t.trim().is_empty() => Text::Fresh,
            Ok(t) if t.starts_with('\u{feff}') || shape_is_unprovable(t) => Text::Unprovable,
            Ok(t) => Text::Provable(t),
            Err(_) => Text::Unprovable,
        },
    }
}

/// Where topos's block sits in a config, or why it does not.
#[derive(Debug, PartialEq, Eq)]
enum Located {
    /// Our block, as the half-open line range `[start, end)`: the sentinel line, the `[[hooks]]`
    /// header it leads, and that table's lines up to the next header (or EOF).
    Managed(usize, usize),
    /// A topos sweep in somebody's own `[[hooks]]` table, sentinel-free — adopt-or-leave.
    Unmanaged,
    /// No topos auto-update block at all.
    Absent,
    /// A shape the anchors refuse to reason about: two sentinel lines (which of them is ours?), or
    /// a sentinel line that does not lead a `[[hooks]]` table.
    Unprovable,
}

fn locate(lines: &[&str]) -> Located {
    let sentinels: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == SENTINEL)
        .map(|(i, _)| i)
        .collect();
    let [start] = sentinels[..] else {
        if sentinels.is_empty() {
            return if holds_a_hand_rolled_sweep(lines) {
                Located::Unmanaged
            } else {
                Located::Absent
            };
        }
        return Located::Unprovable; // more than one — none of them provably ours
    };
    if lines.get(start + 1).and_then(|l| table_header(l)) != Some(HOOKS_HEADER) {
        return Located::Unprovable; // our marker, somebody else's shape under it
    }
    // A TOML table runs to the next header; ours therefore ends there, or at EOF.
    let end = lines[start + 2..]
        .iter()
        .position(|l| table_header(l).is_some())
        .map_or(lines.len(), |offset| start + 2 + offset);
    Located::Managed(start, end)
}

/// Whether a person's OWN `[[hooks]]` entry already invokes a topos sweep. Only a `command`
/// assignment inside a `[[hooks]]` table counts: the same string in another table is a different
/// setting, and reading it as a hook would leave the machine with no trigger at all.
fn holds_a_hand_rolled_sweep(lines: &[&str]) -> bool {
    let mut in_hooks = false;
    for line in lines {
        if table_header(line).is_some() {
            // Their table in whichever spelling TOML reads as `[[hooks]]` — `[["hooks"]]` and
            // `[[ hooks ]]` hold the same entries, and missing one would install a duplicate
            // sweep beside theirs.
            in_hooks =
                header_stated(line, "hooks") == Some(HeaderStated::ArrayOfTables { whole: true });
            continue;
        }
        if in_hooks
            && is_key_line(line, "command")
            && basic_string_value(line).is_some_and(super::is_hand_rolled_sweep)
        {
            return true;
        }
    }
    false
}

/// Plan the block's arming over the existing `config.toml` bytes.
///
/// Two shapes no arming can express, both because the appended `[[hooks]]` would REDEFINE the
/// name and leave kimi unable to read any of its own config: a `hooks` key defined at the file's
/// ROOT (`hooks = []`, `hooks.enabled = false` — the name is then a value), and a header stating
/// `hooks` as a plain table (`[hooks]`, or implicitly via `[hooks.sub]` / `[[hooks.sub]]`). Only
/// the whole-array spelling `[[hooks]]` coexists with ours — one more entry extends an array.
/// Both refusals are INSTALL's alone — [`plan_remove`] appends nothing, and taking our own
/// sentinel-led block back out of such a file can only leave a person closer to the config they
/// meant, so a scrub keeps its full reach.
fn plan_install(current: Option<&[u8]>) -> EditPlan {
    let text = match text_of(current) {
        Text::Fresh => {
            return EditPlan::Write(block().into_bytes(), TriggerState::Active, Some(NOTE));
        }
        Text::Provable(text) => text,
        Text::Unprovable => {
            return EditPlan::Leave(TriggerState::Degraded, Some(crate::UNPROVABLE_REASON));
        }
    };
    let lines = split_lines(text);
    if root_defines_key(&lines, "hooks") {
        return EditPlan::Leave(TriggerState::Degraded, Some(ROOT_HOOKS_REASON));
    }
    if lines.iter().any(|line| {
        matches!(
            header_stated(line, "hooks"),
            Some(HeaderStated::Table { .. } | HeaderStated::ArrayOfTables { whole: false })
        )
    }) {
        return EditPlan::Leave(TriggerState::Degraded, Some(HEADER_HOOKS_REASON));
    }
    match locate(&lines) {
        Located::Unprovable => {
            EditPlan::Leave(TriggerState::Degraded, Some(crate::UNPROVABLE_REASON))
        }
        Located::Unmanaged => EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged, None),
        Located::Managed(start, end) => {
            // Ours already — but a block an EARLIER build wrote may carry a stale command or event.
            // Rewrite the region in place; a true no-op when it already matches.
            let canonical = block();
            if lines[start..end].concat() == canonical {
                return EditPlan::Leave(TriggerState::Active, Some(NOTE));
            }
            let mut out = lines[..start].concat();
            out.push_str(&canonical);
            out.push_str(&lines[end..].concat());
            EditPlan::Write(out.into_bytes(), TriggerState::Active, Some(NOTE))
        }
        Located::Absent => {
            // A fresh table appended at EOF, blank-line separated: a top-level table header is
            // absolute, so an EOF append can never land inside somebody else's table.
            let mut out = text.trim_end_matches('\n').to_owned();
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&block());
            EditPlan::Write(out.into_bytes(), TriggerState::Active, Some(NOTE))
        }
    }
}

fn plan_remove(current: Option<&[u8]>) -> EditPlan {
    let text = match text_of(current) {
        Text::Fresh => return EditPlan::Leave(TriggerState::Inactive, None), // nothing to remove
        Text::Provable(text) => text,
        Text::Unprovable => {
            return EditPlan::Leave(TriggerState::Degraded, Some(crate::UNPROVABLE_REASON));
        }
    };
    let lines = split_lines(text);
    match locate(&lines) {
        Located::Unprovable => {
            EditPlan::Leave(TriggerState::Degraded, Some(crate::UNPROVABLE_REASON))
        }
        Located::Unmanaged => EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged, None),
        Located::Absent => EditPlan::Leave(TriggerState::Inactive, None),
        Located::Managed(start, end) => {
            let mut out = lines[..start].concat();
            out.push_str(&lines[end..].concat());
            // Our block arrived after a blank separator line; take it back out with the block.
            while out.ends_with("\n\n") {
                out.pop();
            }
            if out.trim().is_empty() {
                out.clear(); // the config existed for our block alone
            }
            EditPlan::Write(out.into_bytes(), TriggerState::Inactive, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{ErrConfig, MemConfig};
    use super::*;

    const CONFIG: &str = "/k/config.toml";

    fn a<'c>(cfg: &'c MemConfig) -> KimiCodeCli<'c> {
        KimiCodeCli::new(PathBuf::from("/k"), cfg)
    }

    /// The byte-exact block — pinned as a literal so a drift in the composed `block()` (or the
    /// shared sweep const) fails loudly here.
    const BLOCK_FIXTURE: &str = "\
# topos:currency
[[hooks]]
event = \"SessionStart\"
command = \"command -v topos >/dev/null 2>&1 && topos update --quiet || true\"
timeout = 60
";

    #[test]
    fn the_canonical_block_is_the_sentinel_led_hooks_table() {
        assert_eq!(block(), BLOCK_FIXTURE, "byte-exact fixture");
        assert!(
            block().starts_with(&format!("{SENTINEL}\n")),
            "marker first"
        );
        assert!(
            !GUARDED_SWEEP.contains(['"', '\\']),
            "the command needs no TOML escaping — a basic string carries it verbatim"
        );
        assert_eq!(
            basic_string_value(
                "command = \"command -v topos >/dev/null 2>&1 && topos update --quiet || true\""
            ),
            Some(GUARDED_SWEEP),
            "the command reads back exactly as written"
        );
    }

    #[test]
    fn fresh_install_writes_the_block_and_reports_active() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.agent, "kimi-code-cli");
        assert_eq!(report.marker_id, "topos:kimi-code-cli:currency:1");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.currency_kind, CurrencyKind::SessionStart);
        assert_eq!(report.note.as_deref(), Some("hook shape unverified"));
        assert_eq!(report.touched_path.as_deref(), Some(CONFIG));
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(BLOCK_FIXTURE));
        assert_eq!(cfg.writes(), 1);
    }

    #[test]
    fn install_is_idempotent_a_true_no_op_on_rerun() {
        let cfg = MemConfig::default();
        a(&cfg).install();
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Active);
        assert!(report.touched_path.is_none());
        assert_eq!(cfg.writes(), 1, "second install writes nothing");
    }

    /// The block lands at EOF after a blank line, and every line a person wrote re-emits
    /// byte-for-byte — including a `[[hooks]]` entry of their own, which is joined, never replaced.
    #[test]
    fn the_block_is_appended_and_foreign_content_re_emits_verbatim() {
        let before = "model = \"kimi-k2\"\n\n[[hooks]]\nevent = \"Stop\"\ncommand = \"say done\"\ntimeout = 5\n";
        let cfg = MemConfig::with_file(CONFIG, before);
        a(&cfg).install();
        assert_eq!(
            cfg.text(CONFIG).as_deref(),
            Some(&*format!("{before}\n{BLOCK_FIXTURE}"))
        );

        let report = a(&cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(
            cfg.text(CONFIG).as_deref(),
            Some(before),
            "the config is back to exactly what they wrote"
        );
    }

    /// A user's own table AFTER ours bounds the region: the scrub takes our block and stops at
    /// their header.
    #[test]
    fn a_table_after_our_block_bounds_the_region() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "[[hooks]]\nevent = \"Stop\"\ncommand = \"say done\"\n",
        );
        a(&cfg).install();
        let with_tail = format!(
            "{}\n[tui]\nmouse = false\n",
            cfg.text(CONFIG).unwrap().trim_end_matches('\n')
        );
        cfg.set(CONFIG, &with_tail);

        // A re-arm over the bounded region is still a true no-op.
        assert_eq!(a(&cfg).install().state, TriggerState::Active);
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(&*with_tail));

        a(&cfg).remove();
        assert_eq!(
            cfg.text(CONFIG).as_deref(),
            Some("[[hooks]]\nevent = \"Stop\"\ncommand = \"say done\"\n\n[tui]\nmouse = false\n"),
            "their hook and their table both survive"
        );
    }

    /// A block an earlier build wrote — recognized by the sentinel line alone — is rewritten in
    /// place rather than appended beside.
    #[test]
    fn a_stale_managed_block_migrates_in_place() {
        let stale = "model = \"kimi-k2\"\n\n# topos:currency\n[[hooks]]\nevent = \"SessionStart\"\ncommand = \"topos pull --quiet\"\ntimeout = 10\n";
        let cfg = MemConfig::with_file(CONFIG, stale);
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(cfg.writes(), 1, "one migrating write");
        let text = cfg.text(CONFIG).unwrap();
        assert_eq!(text, format!("model = \"kimi-k2\"\n\n{BLOCK_FIXTURE}"));
        assert_eq!(text.matches(SENTINEL).count(), 1, "never duplicated");
        a(&cfg).install();
        assert_eq!(cfg.writes(), 1, "idempotent after the migration");
    }

    /// Somebody's own sweep in their own `[[hooks]]` entry is adopted-or-left with ZERO writes —
    /// in either verb, and never joined by a managed second copy.
    #[test]
    fn a_hand_rolled_sweep_is_adopt_or_leave() {
        for hand_rolled in ["topos update --quiet", "topos pull"] {
            let theirs =
                format!("[[hooks]]\nevent = \"SessionStart\"\ncommand = \"{hand_rolled}\"\n");
            let cfg = MemConfig::with_file(CONFIG, &theirs);
            assert_eq!(
                a(&cfg).install().state,
                TriggerState::AlreadyPresentUnmanaged,
                "{hand_rolled}"
            );
            assert_eq!(
                a(&cfg).remove().state,
                TriggerState::AlreadyPresentUnmanaged,
                "{hand_rolled}"
            );
            assert_eq!(cfg.writes(), 0, "{hand_rolled}");
            assert_eq!(cfg.text(CONFIG).as_deref(), Some(theirs.as_str()));
            assert!(!a(&cfg).present(), "{hand_rolled}");
        }
    }

    /// A `command` key spelled with quotes states the same key — so somebody's own sweep is still
    /// adopted-or-left instead of being joined by a managed second copy.
    #[test]
    fn a_quoted_command_key_is_still_their_own_sweep() {
        let theirs =
            "[[hooks]]\nevent = \"SessionStart\"\n\"command\" = \"topos update --quiet\"\n";
        let cfg = MemConfig::with_file(CONFIG, theirs);
        assert_eq!(
            a(&cfg).install().state,
            TriggerState::AlreadyPresentUnmanaged
        );
        assert_eq!(cfg.writes(), 0);
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(theirs));
    }

    /// A `[[hooks]]` line inside somebody's multiline string is their prose, not a table — the
    /// whole file is refused rather than read as structure it is not.
    #[test]
    fn a_multiline_string_degrades_instead_of_being_read_as_structure() {
        let prose = "prompt = \"\"\"\n[[hooks]]\ncommand = \"topos update --quiet\"\n\"\"\"\n";
        let cfg = MemConfig::with_file(CONFIG, prose);
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Degraded);
        assert!(report.note.is_some(), "a degrade says why");
        assert_eq!(a(&cfg).remove().state, TriggerState::Degraded);
        assert_eq!(cfg.writes(), 0);
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(prose), "byte-untouched");
        assert!(!a(&cfg).present());
    }

    /// A root-level `hooks` key is the name our `[[hooks]]` header would REDEFINE — arming refuses
    /// with zero writes. A scrub still reaches its own block there, because removal appends
    /// nothing.
    #[test]
    fn a_root_level_hooks_key_degrades_instead_of_redefining_the_name() {
        for root_defined in ["hooks = []\n", "hooks.enabled = false\n"] {
            let cfg = MemConfig::with_file(CONFIG, root_defined);
            let report = a(&cfg).install();
            assert_eq!(report.state, TriggerState::Degraded, "{root_defined:?}");
            assert!(
                report.note.as_deref().unwrap().contains("hooks"),
                "the note names the key that collides"
            );
            assert_eq!(cfg.writes(), 0, "{root_defined:?}");
            assert_eq!(cfg.text(CONFIG).as_deref(), Some(root_defined));
            assert!(!a(&cfg).present());
        }

        // The same key under a table is `tui.hooks` — somebody else's setting entirely.
        let cfg = MemConfig::with_file(CONFIG, "[tui]\nhooks = []\n");
        assert_eq!(a(&cfg).install().state, TriggerState::Active);

        // Removal keeps its reach: our own block comes back out, their key stays.
        let with_ours = format!("hooks = []\n\n{BLOCK_FIXTURE}");
        let cfg = MemConfig::with_file(CONFIG, &with_ours);
        assert_eq!(a(&cfg).remove().state, TriggerState::Inactive);
        assert_eq!(cfg.text(CONFIG).as_deref(), Some("hooks = []\n"));
    }

    /// Review follow-up: a header stating `hooks` as a plain table — directly or implicitly via
    /// a dotted path — is a shape the appended `[[hooks]]` would redefine, so the file degrades
    /// untouched. A whole `[[hooks]]` array coexists (one more entry extends it), so a sweep-free
    /// one still takes the append.
    #[test]
    fn a_hooks_table_header_degrades_instead_of_redefining_the_name() {
        for conflicting in [
            "[hooks]\nx = 1\n",
            "[hooks.sub]\nx = 1\n",
            "[[hooks.sub]]\n",
        ] {
            let cfg = MemConfig::with_file(CONFIG, conflicting);
            let report = a(&cfg).install();
            assert_eq!(report.state, TriggerState::Degraded, "{conflicting:?}");
            assert!(
                report.note.as_deref().unwrap().contains("hooks"),
                "the note names the colliding shape"
            );
            assert_eq!(cfg.writes(), 0, "{conflicting:?}");
            assert_eq!(cfg.text(CONFIG).as_deref(), Some(conflicting));
        }
        let coexists = "[[hooks]]\nevent = \"Stop\"\ncommand = \"echo hi\"\n";
        let cfg = MemConfig::with_file(CONFIG, coexists);
        assert_eq!(a(&cfg).install().state, TriggerState::Active);
    }

    /// Review follow-up: their table in a quoted or spaced spelling is still `[[hooks]]`, so a
    /// hand-rolled sweep inside one stays adopt-or-leave instead of gaining a duplicate beside it.
    #[test]
    fn a_quoted_hooks_array_header_is_still_their_own_table() {
        for theirs in [
            "[[\"hooks\"]]\nevent = \"SessionStart\"\ncommand = \"topos update --quiet\"\n",
            "[[ hooks ]]\ncommand = \"topos update\"\n",
        ] {
            let cfg = MemConfig::with_file(CONFIG, theirs);
            let report = a(&cfg).install();
            assert_eq!(
                report.state,
                TriggerState::AlreadyPresentUnmanaged,
                "{theirs:?}"
            );
            assert_eq!(cfg.writes(), 0, "{theirs:?}");
        }
    }

    /// The same string outside a `[[hooks]]` table is a different setting — reading it as a hook
    /// would leave the machine with no trigger at all, silently.
    #[test]
    fn a_topos_command_in_another_table_is_not_a_hook() {
        let cfg = MemConfig::with_file(CONFIG, "[aliases]\ncommand = \"topos update\"\n");
        assert_eq!(a(&cfg).install().state, TriggerState::Active);
        assert!(cfg.text(CONFIG).unwrap().contains(SENTINEL));
    }

    /// Anything the anchors cannot stand on is left byte-untouched, and the receipt says why.
    #[test]
    fn an_unprovable_config_degrades_with_zero_writes() {
        // Two sentinel lines: which block is ours?
        let doubled = format!("{BLOCK_FIXTURE}\n{BLOCK_FIXTURE}");
        // Our marker over somebody else's shape.
        let misled = "# topos:currency\n[tui]\nmouse = false\n".to_owned();
        for unprovable in [doubled, misled] {
            let cfg = MemConfig::with_file(CONFIG, &unprovable);
            let report = a(&cfg).install();
            assert_eq!(report.state, TriggerState::Degraded, "{unprovable:?}");
            assert!(report.note.is_some(), "a degrade says why");
            assert_eq!(a(&cfg).remove().state, TriggerState::Degraded);
            assert_eq!(cfg.writes(), 0);
            assert_eq!(cfg.text(CONFIG).as_deref(), Some(unprovable.as_str()));
            assert!(!a(&cfg).present());
        }

        // Non-UTF-8 bytes: nothing for the line anchors to stand on.
        let cfg = MemConfig::default();
        cfg.set_raw(CONFIG, b"\xff\xfe not text");
        assert_eq!(a(&cfg).install().state, TriggerState::Degraded);
        assert_eq!(cfg.writes(), 0);

        // A byte-order mark hides the first line's true content.
        let bom = "\u{feff}model = \"kimi-k2\"\n";
        let cfg = MemConfig::with_file(CONFIG, bom);
        assert_eq!(a(&cfg).install().state, TriggerState::Degraded);
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(bom), "byte-untouched");
    }

    #[test]
    fn remove_is_surgical_then_idempotent() {
        let cfg = MemConfig::default();
        a(&cfg).install();
        let report = a(&cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
        assert_eq!(
            cfg.text(CONFIG).as_deref(),
            Some(""),
            "the config existed for our block alone"
        );

        let writes = cfg.writes();
        assert_eq!(a(&cfg).remove().state, TriggerState::Inactive);
        assert_eq!(cfg.writes(), writes, "second remove writes nothing");

        // A remove over an absent install creates nothing.
        let absent = MemConfig::default();
        assert_eq!(a(&absent).remove().state, TriggerState::Inactive);
        assert!(absent.text(CONFIG).is_none());
    }

    #[test]
    fn present_and_artifacts_are_honest() {
        let cfg = MemConfig::default();
        let adapter = a(&cfg);
        assert!(!adapter.present());
        assert!(adapter.artifacts().is_empty());
        adapter.install();
        assert!(adapter.present());
        assert_eq!(
            adapter.artifacts(),
            vec![TriggerArtifact::Path(PathBuf::from(CONFIG))]
        );
        assert_eq!(adapter.config_file().as_deref(), Some(Path::new(CONFIG)));
        adapter.remove();
        assert!(!adapter.present());
        assert!(adapter.artifacts().is_empty());
    }

    #[test]
    fn the_config_lives_under_kimis_own_root() {
        let cfg = MemConfig::default();
        assert_eq!(
            resolve_root(Path::new("/home/me")),
            PathBuf::from("/home/me/.kimi-code")
        );
        assert_eq!(
            adapter(Path::new("/home/me"), &cfg).config_file(),
            Some(PathBuf::from("/home/me/.kimi-code/config.toml"))
        );
    }

    #[test]
    fn an_unreadable_store_degrades_with_zero_writes() {
        let cfg = ErrConfig;
        let adapter = KimiCodeCli::new(PathBuf::from("/k"), &cfg);
        assert_eq!(adapter.install().state, TriggerState::Degraded);
        assert_eq!(adapter.remove().state, TriggerState::Degraded);
        assert!(!adapter.present(), "presence is never claimed on faith");
    }
}
