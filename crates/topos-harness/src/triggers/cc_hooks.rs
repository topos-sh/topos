//! The JSON-config-merge trigger base — Claude-Code-shaped hooks registered in a harness's
//! shared strict-JSON config file. This generalizes the reference adapter's `settings.json`
//! machinery (`claude_code` stays untouched as the reference; this base serves the new
//! instances): parse-or-fail-closed; navigate-or-create the events map; Managed / Unmanaged /
//! Absent classification keyed on the sentinel ALONE; canonical-entry migration in place (so a
//! fix to the managed entry reaches installs that predate it); a surgical remove that prunes
//! only what our removal emptied; a byte-preserving no-op on rerun; and a malformed or
//! wrong-typed file degrading with ZERO writes — a user's differently-shaped config is never
//! coerced or clobbered.
//!
//! Parameterized by [`JsonHooksSpec`]: the config file under the harness root, the JSON path to
//! the events map, the event key spelling, the entry SHAPE (Claude-Code-style matcher groups
//! wrapping handler arrays, or flat per-event entry arrays), the handler fields, and the
//! per-instance placed-state/note policy — a harness that gates hooks behind its own consent
//! reports `Inactive` (the explicit-pull floor) even after a successful write, per the honesty
//! contract in the module root.
//!
//! The merge is pure (bytes in → an edit plan out); the crash-safe write is delegated to the
//! injected [`ConfigStore`], exactly like the reference.

use std::path::PathBuf;

use serde_json::{Map, Value};
use topos_types::{CurrencyKind, TriggerState};

use crate::ConfigStore;

use super::{SENTINEL, SHELL_SWEEP_LINE, TriggerAdapter, TriggerOutcome, outcome};

/// The command-identity substring marking a HAND-ROLLED currency hook — a `topos pull` command
/// present WITHOUT our sentinel, which we adopt-or-leave (never blind-touch, never blind-append
/// beside). Not part of the managed-ours check: ownership keys on [`SENTINEL`] alone.
const COMMAND_IDENTITY: &str = "topos pull";

/// One instance's parameterization of the shared JSON-hooks machinery.
pub(crate) struct JsonHooksSpec {
    /// The registry slug.
    pub(crate) slug: &'static str,
    /// The structured marker identity reported in [`TriggerOutcome::marker_id`].
    pub(crate) marker_id: &'static str,
    /// The config file name under the harness root (e.g. `settings.json`, `hooks.json`).
    pub(crate) config_file: &'static str,
    /// The JSON path (from the root object) to the events map (e.g. the top-level `hooks`).
    pub(crate) events_path: &'static [&'static str],
    /// The event key spelling inside the events map (`SessionStart`, `sessionStart`, …).
    pub(crate) event: &'static str,
    /// Whether matcher-groups wrap handler arrays (the Claude Code shape) or the event array
    /// holds flat handler entries.
    pub(crate) grouped: bool,
    /// Whether the canonical handler carries `"type": "command"`.
    pub(crate) handler_type: bool,
    /// The canonical handler's `timeout` value, where the harness supports one.
    pub(crate) timeout: Option<u64>,
    /// A top-level key seeded ONLY when the file is created from scratch (e.g. a schema
    /// `version`); an existing file's own value is never touched.
    pub(crate) root_seed: Option<(&'static str, u64)>,
    /// What fires when this instance's trigger is provably live.
    pub(crate) live_kind: CurrencyKind,
    /// What a successful registration honestly reports: `Active` only where the written artifact
    /// alone is the evidence; `Inactive` where the harness gates hooks behind its own consent
    /// (the note then names the step still owed).
    pub(crate) placed_state: TriggerState,
    /// The receipt note riding a successful registration (consent owed, or the evidence level).
    pub(crate) note: Option<&'static str>,
}

/// A [`TriggerAdapter`] over one [`JsonHooksSpec`] instance + an injected harness root + the
/// [`ConfigStore`] port.
pub(crate) struct JsonHooks<'a> {
    spec: &'static JsonHooksSpec,
    root: PathBuf,
    cfg: &'a dyn ConfigStore,
}

impl<'a> JsonHooks<'a> {
    /// Construct over an explicit harness root. Production resolves the root through the
    /// instance module's `resolve_root`; tests pass a temp/synthetic dir so no real config is
    /// ever touched.
    pub(crate) fn new(
        spec: &'static JsonHooksSpec,
        root: PathBuf,
        cfg: &'a dyn ConfigStore,
    ) -> Self {
        Self { spec, root, cfg }
    }

    fn config_path(&self) -> PathBuf {
        self.root.join(self.spec.config_file)
    }

    /// Read the current config: `None` if absent, `Err` only on a genuine I/O failure.
    fn read(&self) -> std::io::Result<Option<Vec<u8>>> {
        self.cfg.read(&self.config_path())
    }

    fn out(
        &self,
        state: TriggerState,
        touched: bool,
        note: Option<&'static str>,
    ) -> TriggerOutcome {
        outcome(
            self.spec.slug,
            self.spec.live_kind,
            state,
            touched.then(|| self.config_path().to_string_lossy().into_owned()),
            self.spec.marker_id,
            note,
        )
    }

    /// Apply a planned edit: write through the port (degrading honestly if the write fails) or
    /// leave the file untouched, reporting the planned state.
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

impl TriggerAdapter for JsonHooks<'_> {
    fn slug(&self) -> &'static str {
        self.spec.slug
    }

    fn install(&self) -> TriggerOutcome {
        match self.read() {
            Ok(current) => self.apply(plan_install(self.spec, current.as_deref())),
            // Unreadable (e.g. a permission error) — degrade honestly, never blind-overwrite.
            Err(_) => self.out(TriggerState::Degraded, false, None),
        }
    }

    fn remove(&self) -> TriggerOutcome {
        match self.read() {
            Ok(current) => self.apply(plan_remove(self.spec, current.as_deref())),
            Err(_) => self.out(TriggerState::Degraded, false, None),
        }
    }

    /// Presence = a sentinel-marked entry in a well-formed config, right now. A missing,
    /// unreadable, or malformed file answers `false` — never a claim on faith.
    fn present(&self) -> bool {
        let Ok(Some(bytes)) = self.read() else {
            return false;
        };
        let Ok(root) = serde_json::from_slice::<Value>(&bytes) else {
            return false;
        };
        entries_ref(&root, self.spec)
            .is_some_and(|e| matches!(classify(self.spec, e), Classification::Managed))
    }
}

// ---------------------------------------------------------------------------------------------
// The pure config merge — bytes in → an edit plan out. No I/O; fail-closed on anything that
// cannot be safely interpreted.
// ---------------------------------------------------------------------------------------------

/// What a planned edit does: write the post-image bytes, or leave the file untouched (a true
/// no-op, so an unchanged rerun never re-serializes the user's file). Both carry the state (and
/// the receipt note) to report.
enum EditPlan {
    Write(Vec<u8>, TriggerState, Option<&'static str>),
    Leave(TriggerState, Option<&'static str>),
}

/// How the existing event entries relate to topos's managed one.
#[derive(Debug, PartialEq, Eq)]
enum Classification {
    /// Our sentinel-marked entry is present.
    Managed,
    /// A `topos pull` command exists WITHOUT our sentinel (hand-rolled) — adopt-or-leave.
    Unmanaged,
    /// No topos currency entry at all.
    Absent,
}

/// The parse outcome for the existing config bytes.
enum Parsed {
    /// Absent or whitespace-only — start from a fresh object.
    Fresh,
    /// Parsed JSON.
    Value(Value),
    /// Present but not valid JSON — fail closed (never clobber a file we can't read).
    Malformed,
}

fn parse(current: Option<&[u8]>) -> Parsed {
    match current {
        None => Parsed::Fresh,
        Some(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => Parsed::Fresh,
        Some(bytes) => match serde_json::from_slice::<Value>(bytes) {
            Ok(value) => Parsed::Value(value),
            Err(_) => Parsed::Malformed,
        },
    }
}

fn plan_install(spec: &'static JsonHooksSpec, current: Option<&[u8]>) -> EditPlan {
    let (mut root, fresh) = match parse(current) {
        Parsed::Fresh => (Value::Object(Map::new()), true),
        Parsed::Value(v) => (v, false),
        Parsed::Malformed => return EditPlan::Leave(TriggerState::Degraded, None),
    };
    // Seed the schema key (e.g. `version`) only on a from-scratch file — an existing file's own
    // value, or its deliberate absence, is the user's.
    if fresh
        && let Some((key, seed)) = spec.root_seed
        && let Some(obj) = root.as_object_mut()
    {
        obj.insert(key.to_owned(), Value::from(seed));
    }
    // Navigate to (creating) the event entries; a wrong-typed key anywhere fails closed.
    let Some(entries) = entries_mut(&mut root, spec) else {
        return EditPlan::Leave(TriggerState::Degraded, None);
    };
    match classify(spec, entries) {
        Classification::Managed => {
            // Ours already — but an entry an EARLIER build wrote may carry a stale command or
            // shape (the sentinel is version-agnostic on purpose). Rewrite to the current
            // canonical form in place; a true no-op when it already matches.
            let changed = if spec.grouped {
                migrate_grouped(spec, entries)
            } else {
                migrate_flat(spec, entries)
            };
            if changed {
                match serialize(&root) {
                    Some(bytes) => EditPlan::Write(bytes, spec.placed_state, spec.note),
                    None => EditPlan::Leave(TriggerState::Degraded, None),
                }
            } else {
                EditPlan::Leave(spec.placed_state, spec.note) // already canonical → no write
            }
        }
        Classification::Unmanaged => {
            EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged, None) // leave it
        }
        Classification::Absent => {
            entries.push(canonical_entry(spec));
            match serialize(&root) {
                Some(bytes) => EditPlan::Write(bytes, spec.placed_state, spec.note),
                None => EditPlan::Leave(TriggerState::Degraded, None),
            }
        }
    }
}

fn plan_remove(spec: &'static JsonHooksSpec, current: Option<&[u8]>) -> EditPlan {
    let mut root = match parse(current) {
        Parsed::Fresh => return EditPlan::Leave(TriggerState::Inactive, None), // nothing to remove
        Parsed::Value(v) => v,
        Parsed::Malformed => return EditPlan::Leave(TriggerState::Degraded, None),
    };
    let Some(entries) = entries_existing_mut(&mut root, spec) else {
        return EditPlan::Leave(TriggerState::Inactive, None); // no well-typed event key → nothing ours
    };
    match classify(spec, entries) {
        Classification::Absent => EditPlan::Leave(TriggerState::Inactive, None),
        Classification::Unmanaged => EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged, None),
        Classification::Managed => {
            if spec.grouped {
                scrub_grouped(entries);
            } else {
                entries.retain(|e| !command_of(e).is_some_and(is_managed_command));
            }
            prune_empty(&mut root, spec);
            match serialize(&root) {
                Some(bytes) => EditPlan::Write(bytes, TriggerState::Inactive, None),
                None => EditPlan::Leave(TriggerState::Degraded, None),
            }
        }
    }
}

/// The event entries as a mutable array, creating each object along `events_path` (and the event
/// array) if absent. `None` (caller fails closed) when anything along the way is present but the
/// wrong JSON type — a user's differently-shaped config is never coerced.
fn entries_mut<'v>(root: &'v mut Value, spec: &JsonHooksSpec) -> Option<&'v mut Vec<Value>> {
    let mut obj = root.as_object_mut()?;
    for key in spec.events_path {
        obj = obj
            .entry(*key)
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()?;
    }
    obj.entry(spec.event)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
}

/// The event entries as a mutable array IF the whole path already exists well-typed (never
/// creating anything).
fn entries_existing_mut<'v>(
    root: &'v mut Value,
    spec: &JsonHooksSpec,
) -> Option<&'v mut Vec<Value>> {
    let mut obj = root.as_object_mut()?;
    for key in spec.events_path {
        obj = obj.get_mut(*key)?.as_object_mut()?;
    }
    obj.get_mut(spec.event)?.as_array_mut()
}

/// The event entries as a shared array IF the whole path exists well-typed.
fn entries_ref<'v>(root: &'v Value, spec: &JsonHooksSpec) -> Option<&'v Vec<Value>> {
    let mut obj = root.as_object()?;
    for key in spec.events_path {
        obj = obj.get(*key)?.as_object()?;
    }
    obj.get(spec.event)?.as_array()
}

/// Every command string reachable in the event array under this spec's shape (handlers inside
/// matcher groups, or flat entries). A malformed group/entry is skipped, never an error.
fn commands<'v>(spec: &JsonHooksSpec, entries: &'v [Value]) -> Vec<&'v str> {
    let mut out = Vec::new();
    for entry in entries {
        if spec.grouped {
            if let Some(handlers) = entry.get("hooks").and_then(Value::as_array) {
                out.extend(handlers.iter().filter_map(command_of));
            }
        } else if let Some(cmd) = command_of(entry) {
            out.push(cmd);
        }
    }
    out
}

/// Classify the existing entries against topos's sentinel.
fn classify(spec: &JsonHooksSpec, entries: &[Value]) -> Classification {
    let mut unmanaged = false;
    for cmd in commands(spec, entries) {
        if is_managed_command(cmd) {
            return Classification::Managed;
        }
        if cmd.contains(COMMAND_IDENTITY) {
            unmanaged = true;
        }
    }
    if unmanaged {
        Classification::Unmanaged
    } else {
        Classification::Absent
    }
}

fn command_of(handler: &Value) -> Option<&str> {
    handler.get("command").and_then(Value::as_str)
}

/// Ours iff the command carries our sentinel — the version-agnostic ownership marker. Keying on
/// the sentinel ALONE (never the command text) lets a re-arm recognize an entry an earlier build
/// wrote under a different spelling and rewrite it in place instead of duplicating beside it.
fn is_managed_command(cmd: &str) -> bool {
    cmd.contains(SENTINEL)
}

/// The canonical managed handler object for this instance (keys land alphabetically under
/// `serde_json`'s default map, matching the family's writer style).
fn canonical_handler(spec: &JsonHooksSpec) -> Value {
    let mut m = Map::new();
    if spec.handler_type {
        m.insert("type".to_owned(), Value::String("command".to_owned()));
    }
    m.insert(
        "command".to_owned(),
        Value::String(SHELL_SWEEP_LINE.to_owned()),
    );
    if let Some(timeout) = spec.timeout {
        m.insert("timeout".to_owned(), Value::from(timeout));
    }
    Value::Object(m)
}

/// The canonical entry pushed into the event array: a matcher-free group wrapping the handler
/// (grouped shape — an omitted matcher fires on every event source), or the bare handler (flat).
fn canonical_entry(spec: &JsonHooksSpec) -> Value {
    if spec.grouped {
        serde_json::json!({ "hooks": [canonical_handler(spec)] })
    } else {
        canonical_handler(spec)
    }
}

/// Rewrite every managed handler to the canonical object; on a group whose handlers are ALL ours,
/// shed the group's stale source matcher. A group also holding a user's own handler has OUR
/// handler EXTRACTED into its own canonical matcher-free group (their matcher governs THEIR
/// handlers; leaving ours inside would pin it to their source filter while re-arms no-op
/// forever) — the user's group, handlers, and matcher stay byte-identical. Returns whether
/// anything changed; idempotent, so re-running install after a migration writes nothing.
fn migrate_grouped(spec: &JsonHooksSpec, groups: &mut Vec<Value>) -> bool {
    let mut changed = false;
    // Pass 1: pull our handler OUT of any group also holding a user's handler. Extraction only
    // fires when a foreign handler exists, so the user's group is never emptied.
    for group in groups.iter_mut() {
        let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        let ours = handlers
            .iter()
            .filter(|h| command_of(h).is_some_and(is_managed_command))
            .count();
        if ours > 0 && ours < handlers.len() {
            handlers.retain(|h| !command_of(h).is_some_and(is_managed_command));
            changed = true;
        }
    }
    // Pass 2: every remaining managed handler now lives in an all-ours group — canonicalize the
    // handler objects and shed the group's stale matcher.
    let mut any_managed = false;
    for group in groups.iter_mut() {
        let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut ours = false;
        for handler in handlers.iter_mut() {
            if !command_of(handler).is_some_and(is_managed_command) {
                continue;
            }
            ours = true;
            if *handler != canonical_handler(spec) {
                *handler = canonical_handler(spec);
                changed = true;
            }
        }
        if ours {
            any_managed = true;
            if let Some(obj) = group.as_object_mut()
                && obj.remove("matcher").is_some()
            {
                changed = true;
            }
        }
    }
    // Pass 3: an extraction that left NO managed handler anywhere re-homes it as the canonical
    // group (never a duplicate: this fires only when none remains).
    if !any_managed {
        groups.push(canonical_entry(spec));
        changed = true;
    }
    changed
}

/// Rewrite every managed FLAT entry to the canonical handler object in place. (Flat shape has no
/// groups to extract from — an entry is wholly ours or wholly the user's.)
fn migrate_flat(spec: &JsonHooksSpec, entries: &mut [Value]) -> bool {
    let mut changed = false;
    for entry in entries.iter_mut() {
        if command_of(entry).is_some_and(is_managed_command) && *entry != canonical_handler(spec) {
            *entry = canonical_handler(spec);
            changed = true;
        }
    }
    changed
}

/// Drop every topos-marked handler from the grouped shape, pruning a matcher group ONLY when our
/// removal is what emptied it. A group we never managed — including a pre-existing empty one — is
/// the user's and is left intact.
fn scrub_grouped(groups: &mut Vec<Value>) {
    groups.retain_mut(|group| {
        let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true; // not a well-formed group → leave it untouched
        };
        if !handlers
            .iter()
            .any(|h| command_of(h).is_some_and(is_managed_command))
        {
            return true; // we never managed this group → keep it
        }
        handlers.retain(|h| !command_of(h).is_some_and(is_managed_command));
        !handlers.is_empty() // drop only if removing OUR handler is what emptied the group
    });
}

/// After a removal, drop an emptied event array and then each emptied object back along the
/// events path — but only when WE emptied them (this runs only after our scrub) — so a clean
/// uninstall restores the file toward its pre-install shape without disturbing any sibling key.
fn prune_empty(root: &mut Value, spec: &JsonHooksSpec) {
    fn rec(obj: &mut Map<String, Value>, path: &[&str], event: &str) {
        match path.split_first() {
            None => {
                if obj
                    .get(event)
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
                {
                    obj.remove(event);
                }
            }
            Some((head, rest)) => {
                if let Some(child) = obj.get_mut(*head).and_then(Value::as_object_mut) {
                    rec(child, rest, event);
                }
                if obj
                    .get(*head)
                    .and_then(Value::as_object)
                    .is_some_and(|m| m.is_empty())
                {
                    obj.remove(*head);
                }
            }
        }
    }
    if let Some(obj) = root.as_object_mut() {
        rec(obj, spec.events_path, spec.event);
    }
}

/// Serialize the merged config the family's way: 2-space pretty + a trailing newline, keys in
/// `serde_json`'s default (alphabetical) order. A write happens only on a real change, so any
/// normalization is one-time and action-triggered.
fn serialize(root: &Value) -> Option<Vec<u8>> {
    let mut text = serde_json::to_string_pretty(root).ok()?;
    text.push('\n');
    Some(text.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::super::gemini_cli;
    use super::super::testutil::{ErrConfig, MemConfig};
    use super::*;

    /// The grouped-shape spec the deep-machinery tests run against (the real `gemini-cli` one).
    fn grouped() -> &'static JsonHooksSpec {
        &gemini_cli::SPEC
    }

    fn adapter<'a>(cfg: &'a MemConfig) -> JsonHooks<'a> {
        JsonHooks::new(grouped(), PathBuf::from("/r"), cfg)
    }

    const CONFIG: &str = "/r/settings.json";

    #[test]
    fn install_preserves_foreign_keys_and_sibling_hooks() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\n  \"model\": \"pro\",\n  \"hooks\": {\n    \"PreToolUse\": [{\"matcher\": \"Bash\"}]\n  }\n}\n",
        );
        adapter(&cfg).install();
        let root: Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert_eq!(root["model"], "pro", "foreign top-level key survives");
        assert!(
            root["hooks"]["PreToolUse"].is_array(),
            "sibling hook survives"
        );
        assert_eq!(
            classify(grouped(), entries_ref(&root, grouped()).unwrap()),
            Classification::Managed,
            "our hook was added"
        );
    }

    #[test]
    fn migration_extracts_our_handler_from_a_users_matcher_group() {
        // Our sentinel-marked handler sharing a group with a USER's handler: the migration
        // extracts ours into its own canonical matcher-free group; the user's group, handler,
        // and matcher stay byte-identical.
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"SessionStart\":[{\"matcher\":\"resume\",\"hooks\":[{\"type\":\"command\",\"command\":\"echo mine\"},{\"type\":\"command\",\"command\":\"topos pull --quiet  # topos:currency\"}]}]}}",
        );
        adapter(&cfg).install();
        assert_eq!(cfg.writes(), 1);

        let root: Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        let groups = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "ours relocated into its own group");
        assert_eq!(groups[0]["matcher"], "resume", "the user's matcher kept");
        assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(groups[0]["hooks"][0]["command"], "echo mine");
        assert!(groups[1].get("matcher").is_none(), "ours is matcher-free");
        assert_eq!(
            groups[1]["hooks"][0]["command"].as_str().unwrap(),
            SHELL_SWEEP_LINE
        );

        // Idempotent: the relocated shape is canonical — a rerun writes nothing.
        adapter(&cfg).install();
        assert_eq!(cfg.writes(), 1, "no second write");
        assert_eq!(
            cfg.text(CONFIG).unwrap().matches(SENTINEL).count(),
            1,
            "exactly one managed handler — never duplicated by the relocation"
        );
    }

    #[test]
    fn a_sentinel_carrying_entry_is_claimed_by_the_marker_alone() {
        // A foreign command wearing our sentinel is ours by definition — normalized to the
        // canonical managed command IN PLACE, never duplicated beside.
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"echo nope  # topos:currency\"}]}]}}",
        );
        adapter(&cfg).install();
        assert_eq!(cfg.writes(), 1);
        let root: Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        let groups = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "rewritten in place");
        assert_eq!(
            groups[0]["hooks"][0]["command"].as_str().unwrap(),
            SHELL_SWEEP_LINE
        );
    }

    #[test]
    fn remove_keeps_a_pre_existing_empty_group() {
        // A user's own (empty-hooks) group sits alongside ours; the scrub prunes ONLY the group
        // our removal empties, never the user's pre-existing empty one.
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"SessionStart\":[{\"matcher\":\"resume\",\"hooks\":[]}]}}",
        );
        adapter(&cfg).install();
        adapter(&cfg).remove();
        let root: Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        let groups = entries_ref(&root, grouped()).expect("the user's group survives");
        assert_eq!(groups.len(), 1, "only OUR group was pruned");
        assert_eq!(groups[0]["matcher"], "resume");
    }

    #[test]
    fn a_non_object_root_fails_closed() {
        let cfg = MemConfig::with_file(CONFIG, "[1, 2, 3]\n");
        let report = adapter(&cfg).install();
        assert_eq!(report.state, TriggerState::Degraded);
        assert_eq!(cfg.writes(), 0);
        let report = adapter(&cfg).remove();
        assert_eq!(
            report.state,
            TriggerState::Inactive,
            "nothing ours to scrub"
        );
        assert_eq!(cfg.writes(), 0);
    }

    #[test]
    fn an_unreadable_store_degrades_with_zero_writes() {
        let cfg = ErrConfig;
        let a = JsonHooks::new(grouped(), PathBuf::from("/r"), &cfg);
        assert_eq!(a.install().state, TriggerState::Degraded);
        assert_eq!(a.remove().state, TriggerState::Degraded);
        assert!(!a.present(), "presence is never claimed on faith");
    }
}
