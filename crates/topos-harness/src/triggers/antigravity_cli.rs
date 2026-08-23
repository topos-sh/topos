//! `antigravity-cli` — the auto-update hook in the SHARED `<root>/hooks.json` (production root:
//! `$ANTIGRAVITY_CLI_CONFIG_DIR` else `~/.gemini/config`), under a topos-owned NAMED KEY.
//!
//! Antigravity CLI's hooks file maps a NAME to a block of events, so ownership here is the key
//! rather than an entry inside a shared array: topos owns `"topos"` and writes it wholesale, and
//! every other named key — a person's own, another tool's — is byte-preserved through both install
//! and scrub. A scrub removes the key itself, which is the whole artifact.
//!
//! ```json
//! { "topos": { "PreInvocation": [ { "type": "command", "command": "…", "timeout": 60 } ] } }
//! ```
//!
//! **The handler list is FLAT, and that is load-bearing.** Only the tool events accept the
//! Claude-shaped `matcher`/`hooks` group; sending one under `PreInvocation` would invalidate the
//! whole file — every hook in it, ours and theirs. This engine has no grouped shape to write.
//!
//! **`PreInvocation` fires per PROMPT, not per session** — strictly more often than the session
//! boundary the reported kind names, and no debounce is owed in the hook: the quiet sweep
//! self-throttles client-side (a five-minute TTL by default, plus a single-flight lock), so a
//! redundant invocation costs a lock check and exits.
//!
//! **Evidence level: hook shape unverified** — the config directory, the named-key file layout, the
//! event and the flat-handler constraint are taken from herdr's shipped Antigravity CLI integration
//! (Apache-2.0), which owns its own key in exactly this file; no Antigravity documentation was read
//! here and no live build was probed. Nothing in that integration describes a per-hook consent
//! gate, so a registration reports `Active` carrying the evidence-level note.
//!
//! Ownership still keys on the in-command `# topos:currency` sentinel, so the classification every
//! other engine runs is the one that runs here: a person's own sweep — under any key, including
//! ours — is adopted-or-left with ZERO writes, and a scrub deletes the key only where the sentinel
//! proves it ours.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use topos_types::{CurrencyKind, TriggerState};

use crate::registry;
use crate::{ConfigStore, trigger_report};

use super::cc_hooks::{Parsed, is_managed_command, parse, serialize};
use super::{EditPlan, SHELL_SWEEP_LINE, TriggerAdapter, TriggerArtifact, TriggerReport};

const SLUG: &str = "antigravity-cli";
const MARKER_ID: &str = "topos:antigravity-cli:currency:1";
const NOTE: &str = "hook shape unverified";
/// The shared hooks file under the resolved root.
const CONFIG_FILENAME: &str = "hooks.json";
/// The named key topos owns wholesale (the name is reserved product-wide).
const OWNED_KEY: &str = "topos";
/// The one event registered: the per-prompt boundary Antigravity CLI carries.
const EVENT: &str = "PreInvocation";
const TIMEOUT: u64 = 60;
/// Antigravity CLI's own config-dir override — the one herdr's integration reads.
const ENV_VAR: &str = "ANTIGRAVITY_CLI_CONFIG_DIR";

/// Production root: `$ANTIGRAVITY_CLI_CONFIG_DIR` else `~/.gemini/config` under the passed home.
///
/// This resolves through [`registry::env_override`] — the crate's ONE env read — rather than a
/// [`registry::Root`] variant of its own: `Root` is the vocabulary of the DIRECTORY SPECS baked
/// into registry rows (each variant's tag and `~`-spelling is rendered into the generated agent
/// tables and the manifest `dest` grammar), and no row names this hooks dir. A variant nothing
/// resolves would be vocabulary with no referent.
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    root_under(registry::env_override(ENV_VAR), home)
}

/// The root an override decides — split out so a test can state both answers without touching the
/// process environment.
fn root_under(override_dir: Option<PathBuf>, home: &Path) -> PathBuf {
    override_dir.unwrap_or_else(|| home.join(".gemini").join("config"))
}

pub(crate) fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> AntigravityCli<'a> {
    AntigravityCli::new(resolve_root(home), cfg)
}

/// The `antigravity-cli` [`TriggerAdapter`]: the owned-key merge over the shared hooks file. The
/// JSON reading, the ownership predicate and the writer style are the JSON family's
/// ([`super::cc_hooks`]) — only the ownership BOUNDARY differs.
pub(crate) struct AntigravityCli<'a> {
    root: PathBuf,
    cfg: &'a dyn ConfigStore,
}

impl<'a> AntigravityCli<'a> {
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

    /// Apply a planned edit: write through the port (degrading honestly if the write fails) or
    /// leave the file untouched, reporting the planned state.
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

impl TriggerAdapter for AntigravityCli<'_> {
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

    /// The shared hooks file — named whether or not our key is in it right now, because a file
    /// topos could not read is exactly the one it cannot prove anything about.
    fn config_file(&self) -> Option<PathBuf> {
        Some(self.config_path())
    }

    /// Presence = our key, sentinel-confirmed, in a well-formed file, right now. A missing,
    /// unreadable, or malformed file answers `false` — never a claim on faith.
    fn present(&self) -> bool {
        let Ok(Some(bytes)) = self.read() else {
            return false;
        };
        serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|root| root.as_object().map(classify))
            .is_some_and(|c| c == Classification::Managed)
    }

    /// The shared hooks file, disclosed ONLY while it actually holds our key — and never as a path
    /// `uninstall` deletes (the scrub takes the key, the file stays).
    fn artifacts(&self) -> Vec<TriggerArtifact> {
        if self.present() {
            vec![TriggerArtifact::Path(self.config_path())]
        } else {
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The pure owned-key merge — bytes in → an edit plan out. No I/O; fail-closed on anything that
// cannot be safely interpreted.
// ---------------------------------------------------------------------------------------------

/// How the existing file relates to topos's owned key.
#[derive(Debug, PartialEq, Eq)]
enum Classification {
    /// Our key holds a sentinel-marked command.
    Managed,
    /// A topos sweep exists WITHOUT our sentinel — somebody's own hook, under any key, including
    /// this one. Adopt-or-leave.
    Unmanaged,
    /// No topos auto-update hook at all.
    Absent,
}

/// The canonical block our key holds: ONE event, a FLAT handler list (the grouped `matcher`/`hooks`
/// wrapper is invalid here and is never written).
fn canonical_block() -> Value {
    let mut handler = Map::new();
    handler.insert("type".to_owned(), Value::String("command".to_owned()));
    handler.insert(
        "command".to_owned(),
        Value::String(SHELL_SWEEP_LINE.to_owned()),
    );
    handler.insert("timeout".to_owned(), Value::from(TIMEOUT));
    let mut block = Map::new();
    block.insert(EVENT.to_owned(), Value::Array(vec![Value::Object(handler)]));
    Value::Object(block)
}

/// Every command string reachable under ONE named block — each event's entries read BOTH flat (the
/// shape topos writes, and the only one this event accepts) and grouped (the shape the tool events
/// accept), because a person's own sweep must be recognized wherever they put it. A malformed
/// entry is skipped, never an error.
fn commands_in(block: &Value) -> Vec<&str> {
    let mut out = Vec::new();
    let Some(events) = block.as_object() else {
        return out;
    };
    for entries in events.values().filter_map(Value::as_array) {
        for entry in entries {
            out.extend(entry.get("command").and_then(Value::as_str));
            if let Some(handlers) = entry.get("hooks").and_then(Value::as_array) {
                out.extend(
                    handlers
                        .iter()
                        .filter_map(|h| h.get("command").and_then(Value::as_str)),
                );
            }
        }
    }
    out
}

/// Classify the whole file against topos's sentinel. Ours is the sentinel under OUR key; a
/// sentinel-free sweep anywhere (including under our key, where a person may have written their
/// own) is somebody else's and is left alone.
fn classify(root: &Map<String, Value>) -> Classification {
    if root
        .get(OWNED_KEY)
        .is_some_and(|block| commands_in(block).into_iter().any(is_managed_command))
    {
        return Classification::Managed;
    }
    if root.values().any(|block| {
        commands_in(block)
            .into_iter()
            .any(super::is_hand_rolled_sweep)
    }) {
        return Classification::Unmanaged;
    }
    Classification::Absent
}

fn plan_install(current: Option<&[u8]>) -> EditPlan {
    let mut root = match parse(current) {
        Parsed::Fresh => Value::Object(Map::new()),
        Parsed::Value(v) => v,
        Parsed::Malformed => return EditPlan::Leave(TriggerState::Degraded, None),
    };
    // A file that is not a JSON object is not a hooks file we can shape — fail closed.
    let Some(obj) = root.as_object_mut() else {
        return EditPlan::Leave(TriggerState::Degraded, None);
    };
    if classify(obj) == Classification::Unmanaged {
        return EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged, None);
    }
    // Ours or absent: the key is written WHOLESALE, so an entry an earlier build wrote (recognized
    // by the sentinel, whatever its command or shape) is replaced rather than joined.
    let canonical = canonical_block();
    if obj.get(OWNED_KEY) == Some(&canonical) {
        return EditPlan::Leave(TriggerState::Active, Some(NOTE)); // already canonical → no write
    }
    obj.insert(OWNED_KEY.to_owned(), canonical);
    match serialize(&root) {
        Some(bytes) => EditPlan::Write(bytes, TriggerState::Active, Some(NOTE)),
        None => EditPlan::Leave(TriggerState::Degraded, None),
    }
}

fn plan_remove(current: Option<&[u8]>) -> EditPlan {
    let mut root = match parse(current) {
        Parsed::Fresh => return EditPlan::Leave(TriggerState::Inactive, None), // nothing to remove
        Parsed::Value(v) => v,
        Parsed::Malformed => {
            return EditPlan::Leave(TriggerState::Degraded, Some(crate::UNPROVABLE_REASON));
        }
    };
    let Some(obj) = root.as_object_mut() else {
        return EditPlan::Leave(TriggerState::Inactive, None); // not a hooks file → nothing ours
    };
    match classify(obj) {
        Classification::Absent => EditPlan::Leave(TriggerState::Inactive, None),
        Classification::Unmanaged => EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged, None),
        Classification::Managed => {
            obj.remove(OWNED_KEY);
            match serialize(&root) {
                Some(bytes) => EditPlan::Write(bytes, TriggerState::Inactive, None),
                None => EditPlan::Leave(TriggerState::Degraded, Some(crate::UNPROVABLE_REASON)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{ErrConfig, MemConfig};
    use super::super::{SENTINEL, SHELL_SWEEP_LINE};
    use super::*;

    const CONFIG: &str = "/ag/hooks.json";

    fn a<'c>(cfg: &'c MemConfig) -> AntigravityCli<'c> {
        AntigravityCli::new(PathBuf::from("/ag"), cfg)
    }

    /// The exact bytes a fresh install produces: our named key, one event, ONE FLAT handler.
    const FRESH_INSTALL: &str = "\
{
  \"topos\": {
    \"PreInvocation\": [
      {
        \"command\": \"command -v topos >/dev/null 2>&1 && topos install --quiet || true  # topos:currency\",
        \"timeout\": 60,
        \"type\": \"command\"
      }
    ]
  }
}
";

    #[test]
    fn fresh_install_writes_the_owned_key_and_reports_active() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.agent, "antigravity-cli");
        assert_eq!(report.marker_id, "topos:antigravity-cli:currency:1");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.currency_kind, CurrencyKind::SessionStart);
        assert_eq!(report.note.as_deref(), Some("hook shape unverified"));
        assert_eq!(report.touched_path.as_deref(), Some(CONFIG));
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(FRESH_INSTALL));
        assert_eq!(cfg.writes(), 1);
    }

    /// The grouped `matcher`/`hooks` wrapper is valid only for the tool events — writing one here
    /// would invalidate the whole file, so the engine has no shape that could.
    #[test]
    fn the_handler_list_is_flat_and_never_a_matcher_group() {
        let root: Value = serde_json::from_str(FRESH_INSTALL).expect("valid JSON");
        let entries = root["topos"][EVENT].as_array().expect("a flat array");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].get("hooks").is_none(), "no group wrapper");
        assert!(entries[0].get("matcher").is_none(), "no matcher");
        assert_eq!(entries[0]["type"], "command");
        assert_eq!(entries[0]["command"].as_str().unwrap(), SHELL_SWEEP_LINE);
        assert_eq!(entries[0]["timeout"], 60);
    }

    /// The asymmetry, stated: install CLAIMS the reserved key (a value under `topos` that says
    /// nothing about topos is not a hook anyone can be running), while a scrub deletes only what
    /// the sentinel proves is ours — a delete is the act that has to be certain.
    #[test]
    fn an_unrecognizable_value_under_our_key_is_claimed_by_install_and_left_by_remove() {
        let junk = "{\"topos\": \"whatever this was\"}";
        let cfg = MemConfig::with_file(CONFIG, junk);
        assert_eq!(a(&cfg).install().state, TriggerState::Active);
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(FRESH_INSTALL));

        let cfg = MemConfig::with_file(CONFIG, junk);
        assert_eq!(a(&cfg).remove().state, TriggerState::Inactive);
        assert_eq!(cfg.writes(), 0, "never deleted on a guess");
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(junk));
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

    /// Somebody else's named blocks are the whole point of the owned key: they survive install and
    /// scrub byte-identically, and a scrub takes exactly one key with it.
    #[test]
    fn foreign_named_keys_survive_install_and_scrub_byte_identically() {
        let before = "{\n  \"herdr\": {\n    \"PreInvocation\": [\n      {\n        \"type\": \"command\",\n        \"command\": \"sh /h/state.sh session\",\n        \"timeout\": 10\n      }\n    ]\n  },\n  \"mine\": {\n    \"PreToolUse\": [\n      {\n        \"matcher\": \"Bash\",\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"echo mine\"\n          }\n        ]\n      }\n    ]\n  }\n}\n";
        let cfg = MemConfig::with_file(CONFIG, before);
        a(&cfg).install();

        let after: Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        let original: Value = serde_json::from_str(before).unwrap();
        assert_eq!(after["herdr"], original["herdr"], "another tool's key");
        assert_eq!(
            after["mine"], original["mine"],
            "a person's own key, matcher group and all"
        );
        assert!(after["topos"].is_object(), "ours landed beside them");

        let report = a(&cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        let scrubbed: Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert_eq!(scrubbed, original, "the file is back to exactly their keys");
    }

    /// A block an earlier build wrote — recognized by the sentinel alone, whatever its command,
    /// event or shape — is replaced WHOLESALE rather than joined beside.
    #[test]
    fn a_stale_managed_key_is_rewritten_wholesale() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"topos\":{\"PostInvocation\":[{\"type\":\"command\",\"command\":\"topos pull --quiet  # topos:currency\"}]}}",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(cfg.writes(), 1, "one migrating write");
        let text = cfg.text(CONFIG).unwrap();
        assert_eq!(text, FRESH_INSTALL, "canonical, the stale event gone");
        assert_eq!(text.matches(SENTINEL).count(), 1, "never duplicated");
        a(&cfg).install();
        assert_eq!(cfg.writes(), 1, "idempotent after the migration");
    }

    /// A person's own sweep is adopted-or-left with ZERO writes — under their own key, and under
    /// ours too, where the missing sentinel is what says the block is theirs.
    #[test]
    fn a_hand_rolled_sweep_under_any_key_is_adopt_or_leave() {
        for key in ["mine", "topos"] {
            let theirs = format!(
                "{{\"{key}\":{{\"PreInvocation\":[{{\"type\":\"command\",\"command\":\"topos update --quiet\"}}]}}}}"
            );
            let cfg = MemConfig::with_file(CONFIG, &theirs);
            assert_eq!(
                a(&cfg).install().state,
                TriggerState::AlreadyPresentUnmanaged,
                "{key}"
            );
            assert_eq!(
                a(&cfg).remove().state,
                TriggerState::AlreadyPresentUnmanaged,
                "{key}"
            );
            assert_eq!(
                cfg.writes(),
                0,
                "{key}: never written beside, never deleted"
            );
            assert_eq!(cfg.text(CONFIG).as_deref(), Some(theirs.as_str()));
            assert!(!a(&cfg).present(), "{key}");
        }
    }

    #[test]
    fn an_unprovable_file_degrades_with_zero_writes() {
        for unprovable in ["{ nope", "[1, 2, 3]\n", "\"a string\""] {
            let cfg = MemConfig::with_file(CONFIG, unprovable);
            let report = a(&cfg).install();
            assert_eq!(report.state, TriggerState::Degraded, "{unprovable:?}");
            assert_eq!(cfg.writes(), 0, "{unprovable:?}");
            assert_eq!(cfg.text(CONFIG).as_deref(), Some(unprovable));
            assert!(!a(&cfg).present(), "{unprovable:?}");
        }
        // A remove over an unreadable file says why; over a merely wrong-typed one there is
        // nothing of ours to scrub.
        let malformed = MemConfig::with_file(CONFIG, "{ nope");
        let report = a(&malformed).remove();
        assert_eq!(report.state, TriggerState::Degraded);
        assert!(report.note.is_some(), "a degrade says why");
        assert_eq!(malformed.writes(), 0);
        let wrong_type = MemConfig::with_file(CONFIG, "[1]");
        assert_eq!(a(&wrong_type).remove().state, TriggerState::Inactive);
        assert_eq!(wrong_type.writes(), 0);
    }

    #[test]
    fn remove_is_a_key_deletion_then_idempotent() {
        let cfg = MemConfig::default();
        a(&cfg).install();
        let report = a(&cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
        assert_eq!(
            cfg.text(CONFIG).as_deref(),
            Some("{}\n"),
            "the key is gone; the file stays"
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

    /// The root is Antigravity's SHARED config dir, with its own override honored — never the
    /// `~/.gemini/antigravity-cli` runtime dir the registry row detects on.
    #[test]
    fn the_root_is_the_shared_config_dir_or_its_override() {
        let home = Path::new("/home/me");
        assert_eq!(
            root_under(None, home),
            PathBuf::from("/home/me/.gemini/config")
        );
        assert_eq!(
            root_under(Some(PathBuf::from("/elsewhere/ag")), home),
            PathBuf::from("/elsewhere/ag"),
            "the override wins outright"
        );
    }

    #[test]
    fn an_unreadable_store_degrades_with_zero_writes() {
        let cfg = ErrConfig;
        let adapter = AntigravityCli::new(PathBuf::from("/ag"), &cfg);
        assert_eq!(adapter.install().state, TriggerState::Degraded);
        assert_eq!(adapter.remove().state, TriggerState::Degraded);
        assert!(!adapter.present(), "presence is never claimed on faith");
    }
}
