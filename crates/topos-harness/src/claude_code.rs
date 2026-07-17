//! The `ClaudeCode` reference [`HarnessAdapter`] — discovery, byte-exact placement targeting, and the
//! idempotent session-start **currency trigger** edit of `~/.claude/settings.json`.
//!
//! Content-blind: it reads skill *directories* only to confirm a `SKILL.md` exists (never the bytes,
//! never the frontmatter), and the only file it ever writes is the harness **config** — its own
//! `settings.json` hook entry, never a skill file. The strict-JSON merge here is pure (bytes in → bytes
//! out); the crash-safe write is delegated to the injected [`ConfigStore`] so the one atomic
//! `temp → fsync → rename → fsync-dir` sequence lives in the CLI, not a second copy here.

use std::path::PathBuf;

use serde_json::{Map, Value};
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::{ConfigStore, DiscoveredPlacement, HarnessAdapter, PlacementNaming, PlacementTarget};

/// The user-scope layer label recorded for a discovered/placed Claude Code skill. (`project`,
/// `enterprise`, … become representable later without a contract change — `DiscoveredPlacement.layer`
/// is already `Option<String>`.)
pub(crate) const LAYER_USER: &str = "user";

/// The version-agnostic in-command sentinel marking topos's managed currency hook — a trailing shell
/// comment (the command runs via `sh -c`, so `# …` is inert). Detection matches this PREFIX so a later
/// topos still recognizes (and could migrate) an entry an earlier build wrote.
const SENTINEL: &str = "# topos:currency";

/// The command identity that marks a HAND-ROLLED currency hook — a `topos pull` command present
/// WITHOUT our sentinel, which we adopt-or-leave (never blind-touch). This is NOT part of the
/// managed-ours check any more: ownership keys on the sentinel alone (see [`is_managed_command`]), so
/// our own current `topos update` hook is recognized without enumerating every command spelling here.
const COMMAND_IDENTITY: &str = "topos pull";

/// The structured marker identity reported in [`TriggerReport::marker_id`] — topos + harness id +
/// schema version + command identity. Schema 2 = the async, every-SessionStart-source entry.
const MARKER_ID: &str = "topos:claude-code:currency:2";

/// The exact session-start hook command topos installs. The `command -v topos` guard skips the update
/// when the binary is gone (post-uninstall safety); the trailing `|| true` then makes the whole line
/// exit 0 *regardless* — critically when topos is absent (`command -v` itself exits non-zero, and that
/// code would otherwise become the hook's, which the harness paints as a session-start hook error), and
/// equally when an update degrades (plane down): a best-effort currency sweep must never surface as an
/// error at session start (diagnostics go to `~/.topos/log.jsonl`, never the session). `--quiet` keeps
/// stdout near-empty — a no-change sweep emits nothing; a sweep that changed skill bytes emits the ONE
/// SessionStart hook-output JSON (`reloadSkills`) so Claude Code re-scans its skill dirs same-session
/// (any person-facing line rides that document's context injection). The quiet path also self-throttles
/// (a TTL + single-flight gate in the client), so this command may fire on EVERY SessionStart source —
/// startup, resume, clear, compact — cheaply. The trailing comment is the idempotency sentinel.
const HOOK_COMMAND: &str =
    "command -v topos >/dev/null 2>&1 && topos update --quiet || true  # topos:currency";

/// The per-hook timeout (seconds). A real sweep makes network calls (one delivery call per enrolled
/// workspace, plus fetches when a pointer moved), so this must cover a slow-but-working plane — while a
/// dead or stalling one must never hold the session start hostage: the client bounds its own connect/
/// response/body timeouts and trips a plane-down circuit breaker on the first connect failure, so one
/// minute is generous headroom, not the expected cost. The hook also runs `async` (below), so even the
/// worst case never blocks the session.
const HOOK_TIMEOUT_SECS: u64 = 60;

/// The reference [`HarnessAdapter`] for Claude Code. Holds the resolved config home (injected, so tests
/// point it at a temp dir) and the [`ConfigStore`] port that performs the durable config write.
pub struct ClaudeCode<'a> {
    /// `$CLAUDE_CONFIG_DIR` (Claude Code's own override) else `$HOME/.claude`.
    home: PathBuf,
    cfg: &'a dyn ConfigStore,
}

impl std::fmt::Debug for ClaudeCode<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCode")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl<'a> ClaudeCode<'a> {
    /// Construct over an explicit config home + a config-store port. Production passes
    /// [`ClaudeCode::resolve_home`]; tests pass a temp dir so the real `~/.claude` is never touched.
    #[must_use]
    pub fn new(home: PathBuf, cfg: &'a dyn ConfigStore) -> Self {
        Self { home, cfg }
    }

    /// Resolve Claude Code's config home exactly as Claude Code does: `$CLAUDE_CONFIG_DIR` if set, else
    /// `$HOME/.claude` (falling back to `./.claude` if `$HOME` is unset, mirroring the client's own
    /// home resolution). Editing any other path would touch a `settings.json` Claude Code never reads.
    #[must_use]
    pub fn resolve_home() -> PathBuf {
        if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
            return PathBuf::from(dir);
        }
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".claude")
    }

    fn skills_dir(&self) -> PathBuf {
        self.home.join("skills")
    }

    /// The no-discovery placement dir for a pure follower's first receive. Claude Code invokes a skill by
    /// its FOLDER name, so prefer the skill's real (sanitized) display name; on a collision with a
    /// DIFFERENT existing dir (another skill, or the user's own), disambiguate by the workspace slug; fall
    /// back to the globally-unique `skill_id` (which can never collide). Only a FREE dir is ever chosen,
    /// so a first receive never clobbers an existing skill. The `naming` strings are untrusted and are
    /// sanitized to a single safe component before any join.
    fn follower_placement_dir(&self, skill_id: &str, naming: PlacementNaming<'_>) -> PathBuf {
        let skills = self.skills_dir();
        if let Some(name) = naming.name.and_then(crate::sanitize_skill_dir) {
            let by_name = skills.join(&name);
            if !by_name.exists() {
                return by_name;
            }
            // Collision: a different skill (or the user's own dir) already holds this name. Namespace by
            // the workspace so the two coexist (both parts are already sanitized single components).
            if let Some(ws) = naming.workspace_slug.and_then(crate::sanitize_skill_dir) {
                let namespaced = skills.join(format!("{ws}-{name}"));
                if !namespaced.exists() {
                    return namespaced;
                }
            }
        }
        // Unnamed / unsafe name / every candidate taken → the unique id (a validated single component that
        // can never collide with another skill).
        skills.join(skill_id)
    }

    fn settings_path(&self) -> PathBuf {
        self.home.join("settings.json")
    }

    /// Read the current settings, returning `None` if the file does not exist and `Err` only on a
    /// genuine I/O failure (a permission error, say) — distinct from absent.
    fn read_settings(&self) -> std::io::Result<Option<Vec<u8>>> {
        self.cfg.read(&self.settings_path())
    }

    /// Apply a planned edit: write through the port (degrading honestly if the write fails) or leave the
    /// file untouched, reporting the planned state.
    fn apply(&self, plan: EditPlan) -> TriggerReport {
        match plan {
            EditPlan::Leave(state) => self.report(state, false),
            EditPlan::Write(bytes, state) => {
                match self.cfg.replace(&self.settings_path(), &bytes) {
                    Ok(()) => self.report(state, true),
                    Err(_) => self.report(TriggerState::Degraded, false),
                }
            }
        }
    }

    fn report(&self, state: TriggerState, touched: bool) -> TriggerReport {
        TriggerReport {
            harness: HarnessId::ClaudeCode,
            currency_kind: CurrencyKind::SessionStart,
            touched_path: touched.then(|| self.settings_path().to_string_lossy().into_owned()),
            marker_id: MARKER_ID.to_owned(),
            state,
        }
    }

    /// Whether the managed currency entry is currently present (drives `--footprint` disclosure). A
    /// missing/unreadable/malformed settings file means "not present" — we never claim to own a path we
    /// cannot confirm.
    fn has_managed_entry(&self) -> bool {
        let Ok(Some(bytes)) = self.read_settings() else {
            return false;
        };
        let Ok(root) = serde_json::from_slice::<Value>(&bytes) else {
            return false;
        };
        matches!(
            session_start_ref(&root).map(|ss| classify(ss)),
            Some(Classification::Managed)
        )
    }
}

impl HarnessAdapter for ClaudeCode<'_> {
    fn id(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }

    fn discover(&self) -> Vec<DiscoveredPlacement> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.skills_dir()) else {
            return out; // no skills dir (or unreadable) → nothing discovered, never an error
        };
        for entry in entries.flatten() {
            // The command name is the directory name, so a non-UTF-8 name can't be a skill we manage.
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // Skip dot-prefixed entries: a transient `.topos-staging-*` / `.topos-old-*` dir the
            // materializer builds beside a skill dir is never a real skill, even with a `SKILL.md`
            // inside — so a concurrent discovery during the sub-second swap window can't surface it.
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            // A skill is a directory (follow symlinks — a symlinked skill dir is valid) whose root
            // `SKILL.md` is a regular file. SKILL.md's existence confirms skill-ness — never the
            // frontmatter (all-optional, and we never parse it), so a malformed SKILL.md can't mislead.
            if path.is_dir() && path.join("SKILL.md").is_file() {
                out.push(DiscoveredPlacement {
                    path,
                    layer: Some(LAYER_USER.to_owned()),
                });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path)); // read_dir order is OS-dependent — pin it
        out
    }

    fn placement_for(
        &self,
        skill_id: &str,
        naming: PlacementNaming<'_>,
        discovered: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget {
        match discovered {
            Some(d) => PlacementTarget {
                dir: d.path.clone(),
            },
            // No-discovered default: a pure follower's first-receive baseline records this target.
            None => PlacementTarget {
                dir: self.follower_placement_dir(skill_id, naming),
            },
        }
    }

    fn currency_kind(&self) -> CurrencyKind {
        CurrencyKind::SessionStart
    }

    fn install_currency_trigger(&self) -> TriggerReport {
        match self.read_settings() {
            Ok(current) => self.apply(plan_install(current.as_deref())),
            // Unreadable (e.g. a permission error) — degrade honestly, never blind-overwrite.
            Err(_) => self.report(TriggerState::Degraded, false),
        }
    }

    fn remove_currency_trigger(&self) -> TriggerReport {
        match self.read_settings() {
            Ok(current) => self.apply(plan_remove(current.as_deref())),
            Err(_) => self.report(TriggerState::Degraded, false),
        }
    }

    fn uninstall_footprint(&self) -> Vec<PathBuf> {
        // Disclose the config file ONLY when it actually holds our managed entry — and never as a path
        // `uninstall` will delete (it is scrubbed via `remove_currency_trigger`, the file kept).
        if self.has_managed_entry() {
            vec![self.settings_path()]
        } else {
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The pure settings.json merge — bytes in → an edit plan out. No I/O; fail-closed on anything we
// cannot safely interpret (never coerce or clobber a user's differently-shaped config).
// ---------------------------------------------------------------------------------------------

/// What a planned edit does: write the post-image bytes (and report the resulting state), or leave the
/// file untouched (reporting the observed state — a true no-op, so an unchanged launch never
/// re-serializes / re-alphabetizes the user's file).
enum EditPlan {
    Write(Vec<u8>, TriggerState),
    Leave(TriggerState),
}

/// How the existing `SessionStart` hooks relate to topos's managed entry.
#[derive(Debug, PartialEq, Eq)]
enum Classification {
    /// Our marked hook is present.
    Managed,
    /// A `topos pull` hook exists WITHOUT our marker (hand-rolled) — adopt-or-leave.
    Unmanaged,
    /// No topos currency hook at all.
    Absent,
}

fn plan_install(current: Option<&[u8]>) -> EditPlan {
    let mut root = match parse_settings(current) {
        ParsedSettings::Fresh => Value::Object(Map::new()),
        ParsedSettings::Value(v) => v,
        ParsedSettings::Malformed => return EditPlan::Leave(TriggerState::Degraded),
    };
    // Navigate to (creating) `hooks.SessionStart`; a wrong-typed `hooks`/`SessionStart` fails closed.
    let Some(session_start) = session_start_mut(&mut root) else {
        return EditPlan::Leave(TriggerState::Degraded);
    };
    match classify(session_start) {
        Classification::Managed => {
            // Ours already — but an entry an EARLIER build wrote may carry a stale command string or a
            // stale entry shape (the old `matcher: startup` group; a pre-`async` handler). The sentinel
            // is version-agnostic on purpose, so we still recognize it. Rewrite the managed handler —
            // and, on a group holding ONLY our handler, the group's shape — to the current canonical
            // form; a true no-op (no write) when it already matches. This is how a fix to the managed
            // entry reaches installs that predate it — without it, `Managed` would be an unconditional
            // no-op and the old bytes would live forever.
            if migrate_managed(session_start) {
                match serialize(&root) {
                    Some(bytes) => EditPlan::Write(bytes, TriggerState::Active),
                    None => EditPlan::Leave(TriggerState::Degraded),
                }
            } else {
                EditPlan::Leave(TriggerState::Active) // already canonical → no write
            }
        }
        Classification::Unmanaged => EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged), // leave it
        Classification::Absent => {
            session_start.push(managed_group());
            match serialize(&root) {
                Some(bytes) => EditPlan::Write(bytes, TriggerState::Active),
                None => EditPlan::Leave(TriggerState::Degraded),
            }
        }
    }
}

fn plan_remove(current: Option<&[u8]>) -> EditPlan {
    let mut root = match parse_settings(current) {
        ParsedSettings::Fresh => return EditPlan::Leave(TriggerState::Inactive), // nothing to remove
        ParsedSettings::Value(v) => v,
        ParsedSettings::Malformed => return EditPlan::Leave(TriggerState::Degraded), // leave + warn
    };
    let Some(session_start) = session_start_existing_mut(&mut root) else {
        return EditPlan::Leave(TriggerState::Inactive); // no well-typed SessionStart → nothing ours
    };
    match classify(session_start) {
        Classification::Absent => EditPlan::Leave(TriggerState::Inactive),
        Classification::Unmanaged => EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged),
        Classification::Managed => {
            // Drop every topos-marked handler (any topos version's marker, so an older one can't
            // orphan), pruning a matcher group ONLY when OUR removal is what emptied it. A group we
            // never managed — including a pre-existing empty one — is the user's and is left intact.
            session_start.retain_mut(|group| {
                let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                    return true; // not a well-formed group → leave it untouched
                };
                if !handlers
                    .iter()
                    .any(|h| command_of(h).is_some_and(is_managed_command))
                {
                    return true; // we never managed this group → keep it (incl. a pre-existing empty one)
                }
                handlers.retain(|h| !command_of(h).is_some_and(is_managed_command));
                !handlers.is_empty() // drop only if removing OUR handler is what emptied the group
            });
            prune_empty(&mut root);
            match serialize(&root) {
                Some(bytes) => EditPlan::Write(bytes, TriggerState::Inactive),
                None => EditPlan::Leave(TriggerState::Degraded),
            }
        }
    }
}

/// The parse outcome for the existing settings bytes.
enum ParsedSettings {
    /// Absent or whitespace-only — start from a fresh object.
    Fresh,
    /// Parsed JSON.
    Value(Value),
    /// Present but not valid JSON — fail closed (never clobber a file we can't read).
    Malformed,
}

fn parse_settings(current: Option<&[u8]>) -> ParsedSettings {
    match current {
        None => ParsedSettings::Fresh,
        Some(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => ParsedSettings::Fresh,
        Some(bytes) => match serde_json::from_slice::<Value>(bytes) {
            Ok(value) => ParsedSettings::Value(value),
            Err(_) => ParsedSettings::Malformed,
        },
    }
}

/// `hooks.SessionStart` as a mutable array, creating `hooks` (object) and `SessionStart` (array) if
/// absent. `None` (caller fails closed) when the top level, `hooks`, or `SessionStart` is present but
/// the wrong JSON type — we never coerce a user's differently-shaped config.
fn session_start_mut(root: &mut Value) -> Option<&mut Vec<Value>> {
    let obj = root.as_object_mut()?;
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()?;
    hooks
        .entry("SessionStart")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
}

/// `hooks.SessionStart` as a mutable array IF it already exists and is well-typed (never creating it).
fn session_start_existing_mut(root: &mut Value) -> Option<&mut Vec<Value>> {
    root.as_object_mut()?
        .get_mut("hooks")?
        .as_object_mut()?
        .get_mut("SessionStart")?
        .as_array_mut()
}

/// `hooks.SessionStart` as a shared array IF it exists and is well-typed.
fn session_start_ref(root: &Value) -> Option<&Vec<Value>> {
    root.as_object()?
        .get("hooks")?
        .as_object()?
        .get("SessionStart")?
        .as_array()
}

/// Classify the existing `SessionStart` groups against topos's marker.
fn classify(session_start: &[Value]) -> Classification {
    let mut unmanaged = false;
    for group in session_start {
        let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        for handler in handlers {
            let Some(cmd) = command_of(handler) else {
                continue;
            };
            if is_managed_command(cmd) {
                return Classification::Managed;
            }
            if cmd.contains(COMMAND_IDENTITY) {
                unmanaged = true;
            }
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

/// Ours iff the command carries our sentinel — the version-agnostic ownership marker topos writes.
/// Keying on the sentinel ALONE (never the command text) is what lets a re-arm recognize an entry an
/// earlier build wrote under a different command spelling (e.g. the old `topos pull`) and rewrite it in
/// place, so the current `topos update` command REPLACES it instead of duplicating alongside it.
fn is_managed_command(cmd: &str) -> bool {
    cmd.contains(SENTINEL)
}

/// Rewrite every topos-managed handler to the current canonical handler object (command + timeout +
/// type + async), and — on a group whose handlers are ALL ours — normalize the group to the canonical
/// matcher-free shape (an omitted matcher fires on EVERY SessionStart source: startup, resume, clear,
/// compact — the point of the migration). A group also holding a user's own handler keeps its matcher
/// and grouping untouched (we own our handler, never the user's grouping). Returns whether anything
/// changed; idempotent — a canonical entry is left byte-for-byte, so re-running install after a
/// migration writes nothing. The canonical handler still satisfies [`is_managed_command`] (it keeps the
/// sentinel), so the entry stays classified as ours — this is how a re-arm REPLACES an old
/// `topos pull` / `matcher: startup` managed entry with the current async all-sources one, in place.
fn migrate_managed(session_start: &mut Vec<Value>) -> bool {
    let mut changed = false;
    // Pass 1: pull our handler OUT of any group also holding a user's handler. Their group (and
    // its matcher) governs THEIR handlers; leaving ours inside would pin it to their source
    // filter (e.g. `matcher: resume` — never firing at startup) while re-arms read it as
    // canonical and no-op forever. Extraction only fires when a foreign handler exists, so the
    // user's group is never emptied.
    for group in session_start.iter_mut() {
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
    // handler objects and shed the group's stale source matcher.
    let mut any_managed = false;
    for group in session_start.iter_mut() {
        let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut ours = false;
        for handler in handlers.iter_mut() {
            if !command_of(handler).is_some_and(is_managed_command) {
                continue;
            }
            ours = true;
            if handler != &canonical_handler() {
                *handler = canonical_handler();
                changed = true;
            }
        }
        if ours {
            any_managed = true;
            if let Some(obj) = group.as_object_mut()
                && obj.remove("matcher").is_some()
            {
                changed = true; // an all-ours group sheds its stale source matcher
            }
        }
    }
    // Pass 3: an extraction that left NO managed handler anywhere re-homes it as the canonical
    // matcher-free group (never a duplicate: this fires only when none remains).
    if !any_managed {
        session_start.push(managed_group());
        changed = true;
    }
    changed
}

/// The canonical managed handler object.
fn canonical_handler() -> Value {
    serde_json::json!({
        "type": "command",
        "command": HOOK_COMMAND,
        "timeout": HOOK_TIMEOUT_SECS,
        "async": true
    })
}

/// The group topos appends: NO matcher (an omitted matcher fires on every SessionStart source —
/// startup, resume, clear, compact; the quiet sweep's own TTL makes the redundant fires cheap),
/// carrying the one guarded, async command.
fn managed_group() -> Value {
    serde_json::json!({ "hooks": [ canonical_handler() ] })
}

/// After a removal, drop an emptied `SessionStart` array and then an emptied `hooks` object — but only
/// when WE emptied them — so a clean uninstall restores the file toward its pre-install shape without
/// disturbing any sibling key or hook.
fn prune_empty(root: &mut Value) {
    let Some(obj) = root.as_object_mut() else {
        return;
    };
    let Some(hooks) = obj.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    if hooks
        .get("SessionStart")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        hooks.remove("SessionStart");
    }
    if hooks.is_empty() {
        obj.remove("hooks");
    }
}

/// Serialize the merged config the way Claude Code writes it: 2-space pretty + a trailing newline.
/// Key order is `serde_json`'s default (alphabetical) — we deliberately do NOT enable
/// `serde_json/preserve_order` (a workspace-global feature that would flip every `--json` payload to
/// insertion order and break the committed golden fixtures); a write happens only on a real change, so
/// this is a one-time, action-triggered normalization that matches Claude Code's own writer.
fn serialize(root: &Value) -> Option<Vec<u8>> {
    let mut text = serde_json::to_string_pretty(root).ok()?;
    text.push('\n');
    Some(text.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// An in-memory [`ConfigStore`] for the pure-merge tests (the crash-safe write itself is exercised
    /// by the CLI's `FaultFs` crash sweep, where the real syscalls and fault injection live).
    #[derive(Debug, Default)]
    struct MemConfig {
        content: RefCell<Option<Vec<u8>>>,
        writes: RefCell<u32>,
    }
    impl MemConfig {
        fn with(bytes: &str) -> Self {
            Self {
                content: RefCell::new(Some(bytes.as_bytes().to_vec())),
                writes: RefCell::new(0),
            }
        }
        fn text(&self) -> Option<String> {
            self.content
                .borrow()
                .as_ref()
                .map(|b| String::from_utf8(b.clone()).unwrap())
        }
        fn writes(&self) -> u32 {
            *self.writes.borrow()
        }
    }
    impl ConfigStore for MemConfig {
        fn read(&self, _: &Path) -> std::io::Result<Option<Vec<u8>>> {
            Ok(self.content.borrow().clone())
        }
        fn replace(&self, _: &Path, bytes: &[u8]) -> std::io::Result<()> {
            *self.content.borrow_mut() = Some(bytes.to_vec());
            *self.writes.borrow_mut() += 1;
            Ok(())
        }
    }

    /// A self-cleaning temp dir for the `discover` tests (RAII).
    struct TempHome(PathBuf);
    impl TempHome {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("topos-cc-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn skill(&self, name: &str) {
            let d = self.0.join("skills").join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("SKILL.md"), b"---\nname: x\n---\n# x\n").unwrap();
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn adapter<'a>(home: &Path, cfg: &'a MemConfig) -> ClaudeCode<'a> {
        ClaudeCode::new(home.to_path_buf(), cfg)
    }

    /// The exact bytes a fresh install produces — the byte-compared fixture (2-space pretty, keys
    /// alphabetical, trailing newline; matches Claude Code's own writer). NO matcher — an omitted
    /// matcher fires on every SessionStart source (startup/resume/clear/compact); the handler is
    /// async so the sweep never blocks a session event.
    const FRESH_INSTALL: &str = "\
{
  \"hooks\": {
    \"SessionStart\": [
      {
        \"hooks\": [
          {
            \"async\": true,
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
    fn install_into_absent_settings_writes_the_exact_managed_hook() {
        let cfg = MemConfig::default(); // absent
        let home = PathBuf::from("/nonexistent-claude-home");
        let report = adapter(&home, &cfg).install_currency_trigger();

        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.harness, HarnessId::ClaudeCode);
        assert_eq!(report.currency_kind, CurrencyKind::SessionStart);
        assert_eq!(report.marker_id, MARKER_ID);
        assert!(
            report.touched_path.is_some(),
            "a fresh write touches the file"
        );
        assert_eq!(
            cfg.text().as_deref(),
            Some(FRESH_INSTALL),
            "byte-exact fixture"
        );
        assert_eq!(cfg.writes(), 1);
    }

    #[test]
    fn install_is_idempotent_a_true_no_op_on_rerun() {
        let cfg = MemConfig::default();
        let home = PathBuf::from("/h");
        adapter(&home, &cfg).install_currency_trigger();
        let after_first = cfg.text();

        let report = adapter(&home, &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert!(
            report.touched_path.is_none(),
            "idempotent re-run touches nothing"
        );
        assert_eq!(cfg.writes(), 1, "second install writes nothing");
        assert_eq!(cfg.text(), after_first, "bytes unchanged on re-run");
    }

    #[test]
    fn install_migrates_a_stale_managed_command_to_the_current_one() {
        // An entry an earlier build wrote — the old `topos pull` command, without the `|| true` exit-0
        // tail, carrying our sentinel. Recognized by the sentinel ALONE, so it is still classified as
        // ours and rewritten to the current canonical handler (command + async), the group's stale
        // `startup` matcher shed with it.
        let stale = "command -v topos >/dev/null 2>&1 && topos pull --quiet  # topos:currency";
        let cfg = MemConfig::with(&format!(
            "{{\"hooks\":{{\"SessionStart\":[{{\"matcher\":\"startup\",\"hooks\":[{{\"type\":\"command\",\"command\":\"{stale}\",\"timeout\":60}}]}}]}}}}"
        ));

        let report = adapter(&PathBuf::from("/h"), &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert!(
            report.touched_path.is_some(),
            "a stale entry is rewritten, so the file is touched"
        );
        assert_eq!(cfg.writes(), 1, "exactly one migrating write");

        let root: Value = serde_json::from_str(&cfg.text().unwrap()).unwrap();
        let handler = &root["hooks"]["SessionStart"][0]["hooks"][0];
        let cmd = handler["command"].as_str().unwrap();
        assert_eq!(
            cmd, HOOK_COMMAND,
            "migrated to the current canonical command"
        );
        assert!(
            cmd.ends_with("|| true  # topos:currency"),
            "the migrated command carries the exit-0 tail"
        );
        assert_eq!(handler["async"], true, "the migrated handler runs async");
        assert!(
            root["hooks"]["SessionStart"][0].get("matcher").is_none(),
            "the all-ours group shed its startup matcher — the hook now covers every source"
        );

        // Re-running is now a true no-op — the migration is idempotent.
        let again = adapter(&PathBuf::from("/h"), &cfg).install_currency_trigger();
        assert_eq!(again.state, TriggerState::Active);
        assert!(
            again.touched_path.is_none(),
            "no second write after migration"
        );
        assert_eq!(cfg.writes(), 1, "migration does not re-fire");
    }

    #[test]
    fn install_migrates_the_previous_canonical_shape_to_async_all_sources() {
        // The EXACT entry the previous build wrote (matcher: startup; sync handler; current command).
        // A re-arm rewrites it to the async matcher-free canonical shape — in place, idempotent.
        let cfg = MemConfig::with(&format!(
            "{{\"hooks\":{{\"SessionStart\":[{{\"matcher\":\"startup\",\"hooks\":[{{\"type\":\"command\",\"command\":\"{}\",\"timeout\":60}}]}}]}}}}",
            HOOK_COMMAND.replace('"', "\\\"")
        ));
        let report = adapter(&PathBuf::from("/h"), &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(cfg.writes(), 1, "one migrating write");

        let root: Value = serde_json::from_str(&cfg.text().unwrap()).unwrap();
        let groups = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "rewritten in place, never duplicated");
        assert!(groups[0].get("matcher").is_none(), "matcher shed");
        assert_eq!(groups[0]["hooks"][0]["async"], true);

        let again = adapter(&PathBuf::from("/h"), &cfg).install_currency_trigger();
        assert!(again.touched_path.is_none(), "idempotent after migration");
        assert_eq!(cfg.writes(), 1);
    }

    #[test]
    fn migration_moves_our_handler_out_of_a_users_matcher_filtered_group() {
        // Our sentinel-marked handler sharing a group with a USER's handler: leaving ours inside
        // would pin it to THEIR matcher (here `resume` — never firing at startup/clear/compact)
        // while re-arms no-op forever. The migration EXTRACTS our handler into its own canonical
        // matcher-free group; the user's group, handler, and matcher stay byte-identical.
        let cfg = MemConfig::with(
            "{\"hooks\":{\"SessionStart\":[{\"matcher\":\"resume\",\"hooks\":[{\"type\":\"command\",\"command\":\"echo mine\"},{\"type\":\"command\",\"command\":\"topos pull --quiet  # topos:currency\"}]}]}}",
        );
        let report = adapter(&PathBuf::from("/h"), &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(cfg.writes(), 1);

        let root: Value = serde_json::from_str(&cfg.text().unwrap()).unwrap();
        let groups = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "ours relocated into its own group");
        assert_eq!(
            groups[0]["matcher"], "resume",
            "the user's group keeps its matcher"
        );
        let user_handlers = groups[0]["hooks"].as_array().unwrap();
        assert_eq!(user_handlers.len(), 1, "only the user's handler remains");
        assert_eq!(user_handlers[0]["command"], "echo mine");
        assert!(
            groups[1].get("matcher").is_none(),
            "our relocated group is matcher-free (fires on every source)"
        );
        let ours = groups[1]["hooks"].as_array().unwrap();
        assert_eq!(ours.len(), 1);
        assert_eq!(ours[0]["command"].as_str().unwrap(), HOOK_COMMAND);
        assert_eq!(ours[0]["async"], true);

        // Idempotent: the relocated shape is canonical — a re-run writes nothing.
        let again = adapter(&PathBuf::from("/h"), &cfg).install_currency_trigger();
        assert!(again.touched_path.is_none(), "no second write");
        assert_eq!(cfg.writes(), 1);
        assert_eq!(
            cfg.text().unwrap().matches(SENTINEL).count(),
            1,
            "exactly one managed handler — never duplicated by the relocation"
        );
    }

    #[test]
    fn rearming_over_an_old_pull_hook_replaces_it_with_exactly_one_update_hook() {
        // A config already holding the FULL old managed hook line — the `topos pull --quiet` command
        // carrying our sentinel. Re-arming must recognize it (sentinel alone), rewrite it to the new
        // `topos update` command IN PLACE, and never append a second managed group beside it.
        let old =
            "command -v topos >/dev/null 2>&1 && topos pull --quiet || true  # topos:currency";
        let cfg = MemConfig::with(&format!(
            "{{\"hooks\":{{\"SessionStart\":[{{\"matcher\":\"startup\",\"hooks\":[{{\"type\":\"command\",\"command\":\"{old}\",\"timeout\":60}}]}}]}}}}"
        ));

        let report = adapter(&PathBuf::from("/h"), &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert!(report.touched_path.is_some(), "the old hook is rewritten");
        assert_eq!(cfg.writes(), 1, "exactly one replacing write");

        let text = cfg.text().unwrap();
        assert_eq!(
            text.matches("# topos:currency").count(),
            1,
            "exactly ONE managed hook line — never a duplicate"
        );
        assert!(
            text.contains("topos update --quiet"),
            "rewritten to the new update command"
        );
        assert!(
            !text.contains("topos pull"),
            "no old `topos pull` managed entry remains"
        );

        let root: Value = serde_json::from_str(&text).unwrap();
        let groups = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "still exactly one SessionStart group");
        let handlers = groups[0]["hooks"].as_array().unwrap();
        assert_eq!(handlers.len(), 1, "still exactly one handler");
        assert_eq!(
            handlers[0]["command"].as_str().unwrap(),
            HOOK_COMMAND,
            "the single managed hook is the canonical update command"
        );
    }

    #[test]
    fn install_preserves_foreign_keys_and_other_hooks() {
        let cfg = MemConfig::with(
            "{\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"PreToolUse\": [{\"matcher\": \"Bash\"}]\n  }\n}\n",
        );
        let report = adapter(&PathBuf::from("/h"), &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);

        let root: Value = serde_json::from_str(&cfg.text().unwrap()).unwrap();
        assert_eq!(root["model"], "opus", "foreign top-level key survives");
        assert!(
            root["hooks"]["PreToolUse"].is_array(),
            "sibling hook survives"
        );
        assert_eq!(
            classify(session_start_ref(&root).unwrap()),
            Classification::Managed,
            "our hook was added"
        );
    }

    #[test]
    fn install_leaves_a_hand_rolled_topos_pull_unmanaged() {
        let cfg = MemConfig::with(
            "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"topos pull\"}]}]}}",
        );
        let report = adapter(&PathBuf::from("/h"), &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert!(report.touched_path.is_none());
        assert_eq!(
            cfg.writes(),
            0,
            "never blind-append next to a user's own hook"
        );
    }

    #[test]
    fn install_fails_closed_on_malformed_or_wrong_typed_config() {
        // Malformed JSON → degrade, no write.
        let bad = MemConfig::with("{ this is not json ");
        let r = adapter(&PathBuf::from("/h"), &bad).install_currency_trigger();
        assert_eq!(r.state, TriggerState::Degraded);
        assert_eq!(bad.writes(), 0);
        assert_eq!(
            bad.text().as_deref(),
            Some("{ this is not json "),
            "untouched"
        );

        // `hooks` present but the wrong type → degrade, no write.
        let wrong = MemConfig::with("{\"hooks\": \"oops\"}");
        let r = adapter(&PathBuf::from("/h"), &wrong).install_currency_trigger();
        assert_eq!(r.state, TriggerState::Degraded);
        assert_eq!(wrong.writes(), 0);
    }

    #[test]
    fn a_sentinel_carrying_entry_is_claimed_as_ours_by_the_marker_alone() {
        // Ownership keys on the `# topos:currency` sentinel ALONE — so an entry carrying it is ours
        // regardless of the command text. A foreign command wearing our sentinel is claimed and
        // normalized to the canonical managed command IN PLACE (never duplicated beside it).
        let cfg = MemConfig::with(
            "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"echo nope  # topos:currency\"}]}]}}",
        );
        let report = adapter(&PathBuf::from("/h"), &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(cfg.writes(), 1, "the sentinel-carrying entry is rewritten");

        let root: Value = serde_json::from_str(&cfg.text().unwrap()).unwrap();
        let groups = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(
            groups.len(),
            1,
            "no second group appended — rewritten in place"
        );
        assert_eq!(
            groups[0]["hooks"][0]["command"].as_str().unwrap(),
            HOOK_COMMAND,
            "normalized to the canonical managed command"
        );
    }

    #[test]
    fn remove_scrubs_our_entry_and_keeps_siblings_then_is_idempotent() {
        let cfg = MemConfig::with(
            "{\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"PreToolUse\": [{\"matcher\": \"Bash\"}]\n  }\n}\n",
        );
        let home = PathBuf::from("/h");
        adapter(&home, &cfg).install_currency_trigger();
        assert!(cfg.text().unwrap().contains("topos update"));

        let report = adapter(&home, &cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        let root: Value = serde_json::from_str(&cfg.text().unwrap()).unwrap();
        assert_eq!(root["model"], "opus", "foreign key survives the scrub");
        assert!(
            root["hooks"]["PreToolUse"].is_array(),
            "sibling hook survives"
        );
        assert!(
            session_start_ref(&root).is_none(),
            "our SessionStart group was pruned away (we created it)"
        );

        // Idempotent: a second remove is a clean no-op.
        let writes_before = cfg.writes();
        let report = adapter(&home, &cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(cfg.writes(), writes_before, "second remove writes nothing");
    }

    #[test]
    fn remove_leaves_a_hand_rolled_topos_pull_and_an_absent_file_alone() {
        // A user's own `topos pull` (no marker) is never blind-removed.
        let cfg = MemConfig::with(
            "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"topos pull\"}]}]}}",
        );
        let report = adapter(&PathBuf::from("/h"), &cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0);

        // An absent settings file → a clean no-op, never created.
        let absent = MemConfig::default();
        let report = adapter(&PathBuf::from("/h"), &absent).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(absent.text().is_none(), "remove never creates the file");
    }

    #[test]
    fn remove_keeps_a_pre_existing_empty_session_start_group() {
        // A user's own (empty-hooks) SessionStart group sits alongside ours; the scrub must prune ONLY
        // the group our removal empties, never the user's pre-existing empty one.
        let cfg = MemConfig::with(
            "{\"hooks\":{\"SessionStart\":[{\"matcher\":\"resume\",\"hooks\":[]}]}}",
        );
        let home = PathBuf::from("/h");
        adapter(&home, &cfg).install_currency_trigger(); // appends our dedicated group
        let report = adapter(&home, &cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);

        let root: Value = serde_json::from_str(&cfg.text().unwrap()).unwrap();
        let groups = session_start_ref(&root).expect("the user's SessionStart group survives");
        assert_eq!(groups.len(), 1, "only OUR group was pruned");
        assert_eq!(
            groups[0]["matcher"], "resume",
            "the user's empty group is left intact"
        );
    }

    #[test]
    fn remove_degrades_on_malformed_without_clobbering() {
        let cfg = MemConfig::with("{ not json ");
        let report = adapter(&PathBuf::from("/h"), &cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Degraded);
        assert_eq!(
            cfg.text().as_deref(),
            Some("{ not json "),
            "never clobbered"
        );
    }

    #[test]
    fn discover_finds_skill_dirs_and_ignores_non_skills_without_panic() {
        let home = TempHome::new();
        home.skill("pr-describe");
        home.skill("commit-msg");
        // A dir with no SKILL.md is not a skill.
        std::fs::create_dir_all(home.0.join("skills").join("not-a-skill")).unwrap();
        // A stray file under skills/ is not a skill dir.
        std::fs::write(home.0.join("skills").join("loose.txt"), b"x").unwrap();

        let cfg = MemConfig::default();
        let found = adapter(&home.0, &cfg).discover();
        let names: Vec<String> = found
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["commit-msg", "pr-describe"],
            "sorted, skills only"
        );
        assert!(found.iter().all(|d| d.layer.as_deref() == Some("user")));
    }

    #[test]
    fn discover_on_absent_home_is_empty_not_an_error() {
        let cfg = MemConfig::default();
        let found = adapter(&PathBuf::from("/no-such-claude-home-xyz"), &cfg).discover();
        assert!(found.is_empty());
    }

    #[test]
    fn placement_for_reuses_a_discovered_dir() {
        let cfg = MemConfig::default();
        let a = adapter(&PathBuf::from("/h"), &cfg);
        let disc = DiscoveredPlacement {
            path: PathBuf::from("/h/skills/pr-describe"),
            layer: Some(LAYER_USER.to_owned()),
        };
        assert_eq!(
            a.placement_for("topos_abc", PlacementNaming::default(), Some(&disc))
                .dir,
            PathBuf::from("/h/skills/pr-describe")
        );
    }

    #[test]
    fn placement_names_a_free_folder_by_the_sanitized_display_name() {
        let cfg = MemConfig::default();
        // A home with nothing on disk, so no candidate ever collides.
        let a = adapter(&PathBuf::from("/nonexistent-home"), &cfg);
        let naming = PlacementNaming {
            name: Some("deploy-helper"),
            workspace_slug: Some("acme"),
        };
        assert_eq!(
            a.placement_for("topos_abc", naming, None).dir,
            PathBuf::from("/nonexistent-home/skills/deploy-helper"),
            "a free real name becomes the folder verbatim (so the agent invokes it by name)"
        );
    }

    #[test]
    fn placement_falls_back_to_the_id_for_an_absent_or_unsafe_name() {
        let cfg = MemConfig::default();
        let a = adapter(&PathBuf::from("/h"), &cfg);
        // No name → the validated id.
        assert_eq!(
            a.placement_for("topos_abc", PlacementNaming::default(), None)
                .dir,
            PathBuf::from("/h/skills/topos_abc")
        );
        // A name that sanitizes to nothing → the id (never an empty/unsafe component).
        let junk = PlacementNaming {
            name: Some("../../"),
            workspace_slug: None,
        };
        assert_eq!(
            a.placement_for("topos_abc", junk, None).dir,
            PathBuf::from("/h/skills/topos_abc"),
            "an all-unsafe name never redirects the placement"
        );
        // A traversal-looking name is folded to ONE safe component under skills/ — it can never escape.
        let weird = PlacementNaming {
            name: Some("../evil/x"),
            workspace_slug: None,
        };
        let dir = a.placement_for("topos_abc", weird, None).dir;
        assert_eq!(dir, PathBuf::from("/h/skills/evil-x"));
        assert_eq!(
            dir.strip_prefix("/h/skills").unwrap().components().count(),
            1,
            "the placement is always a single component under the skills dir"
        );
    }

    #[test]
    fn placement_namespaces_by_workspace_on_a_collision_then_falls_back_to_the_id() {
        let home = TempHome::new();
        home.skill("deploy-helper"); // a DIFFERENT skill already holds the plain name
        let cfg = MemConfig::default();
        let a = adapter(&home.0, &cfg);

        // Collision on the plain name → namespaced by the (sanitized) workspace slug, never clobbering.
        let naming = PlacementNaming {
            name: Some("deploy-helper"),
            workspace_slug: Some("robert's workspace"),
        };
        assert_eq!(
            a.placement_for("topos_abc", naming, None).dir,
            home.0
                .join("skills")
                .join("robert-s-workspace-deploy-helper"),
        );

        // Now the namespaced form is taken too → the globally-unique id is the ultimate safe fallback.
        home.skill("robert-s-workspace-deploy-helper");
        assert_eq!(
            a.placement_for("topos_abc", naming, None).dir,
            home.0.join("skills").join("topos_abc"),
            "when every named candidate is taken, the unique id never collides",
        );
    }

    #[test]
    fn sanitize_skill_dir_is_always_one_safe_component_or_none() {
        use crate::sanitize_skill_dir;
        assert_eq!(
            sanitize_skill_dir("deploy-helper").as_deref(),
            Some("deploy-helper")
        );
        assert_eq!(
            sanitize_skill_dir("My Cool Skill!").as_deref(),
            Some("My-Cool-Skill")
        );
        // Separators + traversal can never survive as separators.
        assert_eq!(sanitize_skill_dir("../../evil").as_deref(), Some("evil"));
        assert_eq!(sanitize_skill_dir("a/b\\c").as_deref(), Some("a-b-c"));
        assert_eq!(sanitize_skill_dir("a...b").as_deref(), Some("a-b"));
        assert_eq!(sanitize_skill_dir(".hidden").as_deref(), Some("hidden"));
        // Nothing safe → None (the caller falls back to the id).
        assert_eq!(sanitize_skill_dir(""), None);
        assert_eq!(sanitize_skill_dir(".."), None);
        assert_eq!(sanitize_skill_dir("///"), None);
        // The invariant: a produced name is a single component, never a dot-name.
        for raw in ["../x", "a/b", ".hidden", "..", "x/../y", "  ", "a b c"] {
            if let Some(s) = sanitize_skill_dir(raw) {
                assert!(
                    !s.contains('/') && !s.contains('\\') && s != "." && s != "..",
                    "{raw} -> {s} must be one safe component"
                );
            }
        }
    }

    #[test]
    fn footprint_is_disclosed_only_when_our_entry_is_present() {
        let cfg = MemConfig::default();
        let home = PathBuf::from("/h");
        assert!(
            adapter(&home, &cfg).uninstall_footprint().is_empty(),
            "no entry → nothing disclosed"
        );
        adapter(&home, &cfg).install_currency_trigger();
        assert_eq!(
            adapter(&home, &cfg).uninstall_footprint(),
            vec![PathBuf::from("/h/settings.json")],
            "our entry present → settings.json disclosed (never deleted)"
        );
    }
}
