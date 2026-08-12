//! `codex` — the session-start auto-update hook in `<root>/hooks.json` (production root:
//! `$CODEX_HOME` else `~/.codex`), plus the ONE switch that makes codex read hooks at all:
//! `hooks = true` under `[features]` in `<root>/config.toml`.
//!
//! Two surfaces, one trigger:
//!
//! - **`hooks.json`** is an ordinary member of the strict-JSON family ([`super::cc_hooks`]):
//!   Claude-Code-shaped matcher-free groups under a top-level `"hooks"` map, event key
//!   `"SessionStart"`, handler `{"type": "command", "command": <the guarded sweep>,
//!   "timeout": 60}` — so the sentinel-keyed ownership, adopt-or-leave, in-place migration and
//!   prune-only-what-we-emptied removal are the same machinery every other JSON harness runs.
//! - **`config.toml`** gets `hooks = true` inserted right after an existing top-level
//!   `[features]` header, or appended as a new `[features]` table at EOF. The edit is a
//!   line-anchored merge in the Hermes YAML discipline (no TOML dependency here): every other
//!   line re-emits byte-for-byte, and anything it cannot prove — non-UTF-8 bytes, a leading
//!   byte-order mark hiding the first line's true content — fails closed with ZERO writes.
//!
//! Codex's hooks used to live in a `[[hooks.SessionStart]]` block topos appended to `config.toml`
//! itself. That mechanism is gone. There is no migration and no cleanup: a block an earlier build
//! wrote is dead bytes in a file this adapter no longer writes hooks into.
//!
//! **The feature switch is the one harness-owned knob topos sets.** It is codex's master switch
//! for the mechanism topos was just asked to arm — not its consent: consent is the per-definition
//! trust codex asks for in its own UI, which topos never writes and cannot read. A `hooks` key
//! already under `[features]` is rewritten to `true` rather than duplicated beside; a removal
//! leaves the switch exactly as it found it, because turning codex's whole hook system off would
//! reach far past the one entry a scrub owns.
//!
//! **Evidence level:** the two surfaces and their shapes are taken from herdr's shipped Codex
//! integration (Apache-2.0), which writes this `hooks.json` schema and this `[features]` flag; no
//! live build was probed here.
//!
//! **Consent posture — `Active` is NEVER claimed:** codex gates hooks behind persisted
//! per-definition trust granted in its own UI, and that trust store is not readable evidence. A
//! successful install reports `Inactive` + the explicit-pull floor with a note naming the step
//! still owed; the kind when the hook would fire is `SessionStart`. This adapter never writes
//! codex's trust state.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, TriggerState};

use crate::ConfigStore;

use crate::registry::{self, Root};

use super::cc_hooks::{JsonHooks, JsonHooksSpec};
use super::toml_lines::{is_key_line, split_lines, table_header, terminator};
use super::{TriggerAdapter, TriggerArtifact, TriggerReport};

/// Codex's user config file, under the resolved root — the `[features]` switch lives here.
const CONFIG_FILENAME: &str = "config.toml";

/// The consent step still owed after a successful registration (codex's own trust prompt).
const NOTE: &str =
    "trust the hook inside Codex (it will prompt) — until then, explicit `topos update`";

/// The hook entry itself. Schema 2 = the `hooks.json` entry (schema 1 was the retired
/// `config.toml` block). `placed_state` is `Inactive` by construction: codex's trust prompt is
/// owed no matter how the write went.
pub(crate) static SPEC: JsonHooksSpec = JsonHooksSpec {
    slug: "codex",
    marker_id: "topos:codex:currency:2",
    config_file: "hooks.json",
    events_path: &["hooks"],
    event: "SessionStart",
    grouped: true,
    matcher: None,
    handler_type: true,
    command_key: "command",
    timeout: Some(("timeout", 60)),
    handler_async: false,
    // Unmarked, deliberately: codex validates hook output against a strict schema and fails the
    // whole session-start hook on an unknown field.
    hook_dialect: None,
    root_seed: None,
    live_kind: CurrencyKind::SessionStart, // what fires when live; never reported live (see above)
    placed_state: TriggerState::Inactive,
    note: Some(NOTE),
};

/// The `codex` [`TriggerAdapter`]: the shared JSON-hooks engine over `hooks.json`, wrapped with
/// the `config.toml` feature-switch edit. The wrapper owns nothing the base does — every
/// classification, ownership and scrub decision about the entry is the base's.
pub(crate) struct Codex<'a> {
    hooks: JsonHooks<'a>,
    root: PathBuf,
    cfg: &'a dyn ConfigStore,
}

/// Production root: `$CODEX_HOME` (codex's own override, resolved the way the registry does)
/// else `~/.codex` under the passed home.
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    registry::config_root(Root::CodexHome, home)
}

pub(crate) fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> Codex<'a> {
    Codex::new(resolve_root(home), cfg)
}

impl<'a> Codex<'a> {
    pub(crate) fn new(root: PathBuf, cfg: &'a dyn ConfigStore) -> Self {
        Self {
            hooks: JsonHooks::new(&SPEC, root.clone(), cfg),
            root,
            cfg,
        }
    }

    fn config_path(&self) -> PathBuf {
        self.root.join(CONFIG_FILENAME)
    }

    /// Set `[features] hooks = true` in `config.toml`. `Ok(true)` = the file was written,
    /// `Ok(false)` = it already said so (a true no-op), `Err(reason)` = unreadable or an
    /// unprovable shape, ZERO writes either way.
    fn arm_hooks_feature(&self) -> Result<bool, &'static str> {
        let path = self.config_path();
        let current = self.cfg.read(&path).map_err(|e| crate::io_reason(&e))?;
        match plan_feature(current.as_deref()) {
            FeaturePlan::Leave => Ok(false),
            FeaturePlan::Unprovable => Err(crate::UNPROVABLE_REASON),
            FeaturePlan::Write(bytes) => match self.cfg.replace(&path, &bytes) {
                Ok(()) => Ok(true),
                Err(e) => Err(crate::io_reason(&e)),
            },
        }
    }

    fn degraded(&self, touched: Option<String>, reason: &'static str) -> TriggerReport {
        crate::trigger_report(
            SPEC.slug,
            SPEC.live_kind,
            TriggerState::Degraded,
            touched,
            SPEC.marker_id,
            Some(reason),
        )
    }
}

impl TriggerAdapter for Codex<'_> {
    fn slug(&self) -> &'static str {
        "codex"
    }

    /// The entry first, the switch second. A registration that could not place the entry never
    /// touches `config.toml` at all: enabling codex's hook system is only ever a step toward
    /// arming OUR trigger, never a thing done on its own.
    fn install(&self) -> TriggerReport {
        let mut report = self.hooks.install();
        if !matches!(report.state, TriggerState::Active | TriggerState::Inactive) {
            return report; // Degraded, or somebody's own hook adopted-or-left
        }
        match self.arm_hooks_feature() {
            // The entry's own path stays the disclosed one when both were written; a run that
            // only had the switch left to set names the file it actually touched.
            Ok(wrote) => {
                if wrote && report.touched_path.is_none() {
                    report.touched_path = Some(self.config_path().to_string_lossy().into_owned());
                }
                report
            }
            // The entry may well be on disk — but a trigger codex will not read is not armed, and
            // the receipt names the file a person has to fix.
            Err(reason) => self.degraded(report.touched_path, reason),
        }
    }

    /// Scrub the entry; leave the feature switch. `hooks = true` governs codex's whole hook
    /// system, so turning it off on the way out could silence hooks that were never ours.
    fn remove(&self) -> TriggerReport {
        self.hooks.remove()
    }

    /// `hooks.json` — the file our entry lives in, and the one a degraded scrub points a person
    /// at. (`config.toml` holds a shared switch, not our artifact.)
    fn config_file(&self) -> Option<PathBuf> {
        self.hooks.config_file()
    }

    /// Presence = our sentinel-marked entry in a well-formed `hooks.json`, right now. The feature
    /// switch is deliberately not part of the answer: a person who turned it off still has our
    /// entry sitting there, and a scrub must keep reaching it.
    fn present(&self) -> bool {
        self.hooks.present()
    }

    /// `hooks.json` while it holds our entry — and never `config.toml`, whose switch a scrub
    /// leaves standing.
    fn artifacts(&self) -> Vec<TriggerArtifact> {
        self.hooks.artifacts()
    }
}

// ---------------------------------------------------------------------------------------------
// The pure line-anchored TOML merge for the feature switch — bytes in → an edit plan out. No
// I/O; no TOML parse: exactly the shapes provable by line anchors, everything else fails closed.
// ---------------------------------------------------------------------------------------------

/// What the feature-switch edit does: write the post-image bytes, leave the file untouched (it
/// already says `hooks = true`), or refuse to reason about it at all.
enum FeaturePlan {
    Write(Vec<u8>),
    Leave,
    Unprovable,
}

/// The switch as topos writes it, and the whole of a from-scratch `config.toml`.
const FEATURE_TABLE: &str = "[features]\nhooks = true\n";
const FEATURE_KEY_LINE: &str = "hooks = true";

/// Whether an assignment line already states `true` — the value read up to a trailing comment (a
/// boolean can hold neither a quote nor a `#`). A switch already on is left ALONE rather than
/// re-stated, so a person's indentation and their comment beside it survive every re-arm.
fn states_true(line: &str) -> bool {
    line.split_once('=')
        .is_some_and(|(_, value)| value.split('#').next().unwrap_or("").trim() == "true")
}

/// Plan the `[features] hooks = true` edit over the existing `config.toml` bytes.
///
/// Absent or blank → the table IS the file. Otherwise: an existing `hooks` key under the
/// top-level `[features]` table is set to `true` in place; else the line is inserted directly
/// after the first `[features]` header (a key must land inside its own table, and the top is the
/// one position that is always inside it); else the table is appended at EOF, where a fresh
/// top-level table can never fall into somebody else's.
fn plan_feature(current: Option<&[u8]>) -> FeaturePlan {
    let text = match current {
        None => return FeaturePlan::Write(FEATURE_TABLE.into()),
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(t) if t.trim().is_empty() => return FeaturePlan::Write(FEATURE_TABLE.into()),
            Ok(t) => t,
            Err(_) => return FeaturePlan::Unprovable,
        },
    };
    // A byte-order mark hides the first line's true content from the line anchors — never
    // reasoned about.
    if text.starts_with('\u{feff}') {
        return FeaturePlan::Unprovable;
    }

    let lines = split_lines(text);
    let mut header_at = None;
    let mut key_at = None;
    let mut in_features = false;
    for (i, line) in lines.iter().enumerate() {
        if let Some(header) = table_header(line) {
            in_features = header == "[features]";
            if in_features && header_at.is_none() {
                header_at = Some(i);
            }
            continue;
        }
        if in_features && key_at.is_none() && is_key_line(line, "hooks") {
            key_at = Some(i);
        }
    }

    if let Some(i) = key_at {
        if states_true(lines[i]) {
            return FeaturePlan::Leave; // already on → a true no-op, whatever its spelling
        }
        // The switch is codex's, and this is the line that states it: set it, never duplicate it.
        let replacement = format!("{FEATURE_KEY_LINE}{}", terminator(lines[i]));
        let mut out = String::with_capacity(text.len() + replacement.len());
        for (idx, line) in lines.iter().enumerate() {
            if idx == i {
                out.push_str(&replacement);
            } else {
                out.push_str(line);
            }
        }
        return FeaturePlan::Write(out.into_bytes());
    }

    if let Some(i) = header_at {
        let term = terminator(lines[i]);
        let mut out = String::with_capacity(text.len() + FEATURE_KEY_LINE.len() + 2);
        for (idx, line) in lines.iter().enumerate() {
            out.push_str(line);
            if idx == i {
                // A header that ends the file unterminated needs one before the key can follow.
                if !line.ends_with('\n') {
                    out.push_str(term);
                }
                out.push_str(FEATURE_KEY_LINE);
                out.push_str(term);
            }
        }
        return FeaturePlan::Write(out.into_bytes());
    }

    // No `[features]` table at all: append one at EOF, separated by a blank line (a top-level
    // table header is absolute, so an EOF append can never land inside another table).
    let mut out = text.trim_end_matches('\n').to_owned();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(FEATURE_TABLE);
    FeaturePlan::Write(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{ErrConfig, MemConfig};
    use super::super::{SENTINEL, SHELL_SWEEP_LINE};
    use super::*;

    fn a<'c>(cfg: &'c MemConfig) -> Codex<'c> {
        Codex::new(PathBuf::from("/x"), cfg)
    }

    const HOOKS: &str = "/x/hooks.json";
    const CONFIG: &str = "/x/config.toml";

    /// The byte-exact entry a fresh install writes, pinned as a literal so a drift in the shared
    /// consts or the family's writer style fails loudly here.
    const HOOKS_FIXTURE: &str = "\
{
  \"hooks\": {
    \"SessionStart\": [
      {
        \"hooks\": [
          {
            \"command\": \"command -v topos >/dev/null 2>&1 && topos update --quiet || true  # topos:currency\",
            \"timeout\": 60,
            \"type\": \"command\"
          }
        ]
      }
    ]
  }
}
";

    #[test]
    fn fresh_install_writes_the_entry_and_the_switch_and_never_claims_active() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.agent, "codex");
        assert_eq!(report.marker_id, "topos:codex:currency:2");
        // Codex gates hooks behind its own trust prompt — Active is never claimed.
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
        assert!(report.note.as_deref().unwrap().contains("trust the hook"));
        assert_eq!(
            report.touched_path.as_deref(),
            Some(HOOKS),
            "the entry's own file is the disclosed one"
        );
        assert_eq!(cfg.text(HOOKS).as_deref(), Some(HOOKS_FIXTURE));
        assert_eq!(
            cfg.text(CONFIG).as_deref(),
            Some("[features]\nhooks = true\n")
        );
        assert_eq!(cfg.writes(), 2, "one file each, no more");
    }

    #[test]
    fn install_is_idempotent_a_true_no_op_on_rerun() {
        let cfg = MemConfig::default();
        a(&cfg).install();
        let after_first = (cfg.text(HOOKS), cfg.text(CONFIG));
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(
            report.note.is_some(),
            "the consent note rides the no-op too"
        );
        assert!(report.touched_path.is_none());
        assert_eq!(cfg.writes(), 2, "second install writes nothing");
        assert_eq!((cfg.text(HOOKS), cfg.text(CONFIG)), after_first);
    }

    /// The switch goes directly under an existing `[features]` header — inside the table it
    /// belongs to — and every other line of the user's config re-emits byte-for-byte.
    #[test]
    fn the_switch_lands_inside_an_existing_features_table() {
        let before =
            "model = \"gpt-5-codex\"\n\n[features]\nweb_search = true\n\n[tui]\nmouse = false\n";
        let cfg = MemConfig::with_file(CONFIG, before);
        a(&cfg).install();
        assert_eq!(
            cfg.text(CONFIG).as_deref(),
            Some(
                "model = \"gpt-5-codex\"\n\n[features]\nhooks = true\nweb_search = true\n\n[tui]\nmouse = false\n"
            )
        );
    }

    /// With no `[features]` table the whole table is appended at EOF, after a blank line and an
    /// added terminator for an unterminated last line.
    #[test]
    fn the_switch_appends_a_features_table_at_eof() {
        let cfg = MemConfig::with_file(CONFIG, "model = \"gpt-5-codex\""); // no trailing newline
        a(&cfg).install();
        assert_eq!(
            cfg.text(CONFIG).as_deref(),
            Some("model = \"gpt-5-codex\"\n\n[features]\nhooks = true\n")
        );
    }

    /// A `hooks` key already under `[features]` is the line topos sets — never a duplicate key
    /// beside it, which is what would break codex's parse. A `hooks` key in some OTHER table is
    /// somebody else's setting and does not count.
    #[test]
    fn an_existing_hooks_key_is_set_in_place() {
        let cfg = MemConfig::with_file(CONFIG, "[features]\nhooks = false\n");
        a(&cfg).install();
        let after = cfg.text(CONFIG).unwrap();
        assert_eq!(after, "[features]\nhooks = true\n");
        assert_eq!(after.matches("hooks =").count(), 1, "set, not duplicated");

        // Already true → the config is not rewritten at all (only the entry is written), and a
        // switch a person spelled their own way — indented, with their comment beside it — is
        // left exactly as they wrote it.
        for already_on in [
            "[features]\nhooks = true\nweb_search = true\n",
            "[features]\n  hooks = true  # on purpose\n",
        ] {
            let cfg = MemConfig::with_file(CONFIG, already_on);
            a(&cfg).install();
            assert_eq!(cfg.text(CONFIG).as_deref(), Some(already_on));
            assert_eq!(cfg.writes(), 1, "the entry only");
        }

        // A same-named key under a different table is not the switch.
        let cfg = MemConfig::with_file(CONFIG, "[sandbox]\nhooks = false\n");
        a(&cfg).install();
        assert_eq!(
            cfg.text(CONFIG).as_deref(),
            Some("[sandbox]\nhooks = false\n\n[features]\nhooks = true\n")
        );
    }

    /// CRLF in, CRLF out: an inserted line takes the terminator of the line it follows.
    #[test]
    fn an_inserted_line_keeps_the_files_own_line_endings() {
        let cfg = MemConfig::with_file(CONFIG, "[features]\r\nweb_search = true\r\n");
        a(&cfg).install();
        assert_eq!(
            cfg.text(CONFIG).as_deref(),
            Some("[features]\r\nhooks = true\r\nweb_search = true\r\n")
        );
    }

    /// A config topos cannot reason about is never written — and the whole registration says so,
    /// because an entry codex will not read is not an armed trigger.
    #[test]
    fn an_unprovable_config_degrades_and_is_never_rewritten() {
        // Non-UTF-8 bytes: nothing for the line anchors to stand on.
        let cfg = MemConfig::default();
        cfg.set_raw(CONFIG, b"\xff\xfe not text");
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Degraded);
        assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
        assert!(report.note.is_some(), "a degrade says why");
        assert_eq!(
            report.touched_path.as_deref(),
            Some(HOOKS),
            "the entry that DID land is still disclosed"
        );
        assert_eq!(cfg.writes(), 1, "the entry only — config.toml is untouched");

        // A byte-order mark hides the first line's true content from the anchors.
        let bom = "\u{feff}[features]\nweb_search = true\n";
        let cfg = MemConfig::with_file(CONFIG, bom);
        assert_eq!(a(&cfg).install().state, TriggerState::Degraded);
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(bom), "byte-untouched");
    }

    /// A malformed `hooks.json` stops the whole install: the switch is only ever set on the way
    /// to arming our own entry, never on its own.
    #[test]
    fn a_malformed_hooks_file_degrades_before_the_switch_is_touched() {
        let cfg = MemConfig::with_file(HOOKS, "{ nope");
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Degraded);
        assert_eq!(cfg.writes(), 0);
        assert!(cfg.text(CONFIG).is_none(), "config.toml is never created");
    }

    /// Somebody's own sweep hook is adopted-or-left — and their machine's `config.toml` stays
    /// untouched too, since topos armed nothing there.
    #[test]
    fn a_hand_rolled_topos_hook_is_adopt_or_leave() {
        let cfg = MemConfig::with_file(
            HOOKS,
            "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"topos update --quiet\"}]}]}}",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0);
        assert!(cfg.text(CONFIG).is_none());
        assert_eq!(
            a(&cfg).remove().state,
            TriggerState::AlreadyPresentUnmanaged
        );
    }

    /// A stale managed entry — recognized by the sentinel alone — is rewritten in place.
    #[test]
    fn a_stale_managed_entry_migrates_in_place() {
        let cfg = MemConfig::with_file(
            HOOKS,
            "{\"hooks\":{\"SessionStart\":[{\"matcher\":\"startup\",\"hooks\":[{\"type\":\"command\",\"command\":\"topos pull --quiet  # topos:currency\"}]}]}}",
        );
        assert_eq!(a(&cfg).install().state, TriggerState::Inactive);
        let text = cfg.text(HOOKS).unwrap();
        assert_eq!(text.matches(SENTINEL).count(), 1, "never duplicated");
        assert!(text.contains(SHELL_SWEEP_LINE));
    }

    /// The scrub takes our entry and leaves the switch standing: `hooks = true` governs codex's
    /// whole hook system, and hooks that were never ours may depend on it.
    #[test]
    fn remove_scrubs_the_entry_and_leaves_the_switch_then_is_idempotent() {
        let before = "model = \"gpt-5-codex\"\n";
        let cfg = MemConfig::with_file(CONFIG, before);
        a(&cfg).install();

        let report = a(&cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
        let root: serde_json::Value = serde_json::from_str(&cfg.text(HOOKS).unwrap()).unwrap();
        assert!(
            root.get("hooks").is_none(),
            "the map we created is pruned back"
        );
        assert_eq!(
            cfg.text(CONFIG).as_deref(),
            Some("model = \"gpt-5-codex\"\n\n[features]\nhooks = true\n"),
            "the feature switch survives the scrub"
        );

        let writes = cfg.writes();
        assert_eq!(a(&cfg).remove().state, TriggerState::Inactive);
        assert_eq!(cfg.writes(), writes, "second remove writes nothing");

        // A remove over an absent install is a clean no-op that creates nothing.
        let absent = MemConfig::default();
        assert_eq!(a(&absent).remove().state, TriggerState::Inactive);
        assert!(absent.text(HOOKS).is_none());
        assert!(absent.text(CONFIG).is_none());
    }

    /// Disclosure: the entry's file, only while it holds our entry — never `config.toml`, which
    /// a scrub does not reach.
    #[test]
    fn only_the_hooks_file_is_ever_disclosed() {
        let cfg = MemConfig::default();
        let adapter = a(&cfg);
        assert!(adapter.artifacts().is_empty());
        adapter.install();
        assert_eq!(
            adapter.artifacts(),
            vec![TriggerArtifact::Path(PathBuf::from(HOOKS))]
        );
        assert_eq!(adapter.config_file().as_deref(), Some(Path::new(HOOKS)));
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
