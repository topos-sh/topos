//! The `OpenClaw` [`HarnessAdapter`] — discovery, byte-exact placement targeting, and the idempotent
//! **currency trigger** edit of `~/.openclaw/openclaw.json` plus the topos-owned bootstrap-inject
//! plugin file.
//!
//! Content-blind, like the reference: it reads skill *directories* only to confirm a `SKILL.md`
//! exists (never the bytes, never the frontmatter), and the only files it ever writes are OpenClaw's
//! own **config surface** — the `openclaw.json` registration plus the inert topos-owned inject
//! plugin — never a skill dir. The strict-JSON merge is pure (bytes in → an edit plan out); the
//! crash-safe write is delegated to the injected [`ConfigStore`] (which writes *through* a symlink
//! to its target, never replacing the link).
//!
//! **OpenClaw currency is honestly weaker than Claude Code's.** The registered inject file shows its
//! LAST-REFRESHED state, so an update surfaces on the **first `topos` touch** of a session — an
//! honest one-beat-in latency — never at bare session open. OpenClaw's `session_start` surface is
//! observer-only (it cannot inject into LLM context) and a cron job's stdout never reaches context,
//! so neither is ever a currency path here, including as a fallback: every failure mode degrades
//! plainly to an explicit `topos update` (`TriggerState::Degraded` + the `ExplicitPullOnly` floor).
//!
//! READINESS PROBE — this adapter is build-first behind the frozen trait: the concrete config bytes
//! below are PROVISIONAL until verified against the pilot's EXACT OpenClaw build (not latest-main).
//! A failed or absent probe keeps this structure and the honest "next `topos` touch" degrade — never
//! a bare-open currency claim, never a cron. The CLI's one harness-selection site still wires Claude
//! Code only, so no production path selects this adapter before the probe pins the bytes. To verify:
//!   1. bootstrap-inject is enabled and the extra-files registration is honored at gateway bootstrap
//!      (and whether a build could ship it off by default — if so, an absent flag must degrade).
//!   2. the gateway auto-watches `openclaw.json`, so a fresh registration takes effect without a
//!      restart.
//!   3. the extra-files array tolerates the fresh-array (immutable-replace) edit form that sidesteps
//!      upstream openclaw#51789 (in-place array mutation).
//!   4. char-budget headroom: the inject content fits the injected-context budget
//!      ([`INJECT_CHAR_BUDGET`]) — else we degrade, never truncate.
//!   5. the config path is `~/.openclaw/openclaw.json`, the key is [`EXTRA_FILES_KEY`], entries are
//!      plain path strings, and the plugin's file format (`.mjs`) + flat location under
//!      `~/.openclaw/` are what this adapter writes; `$HOME/.openclaw` is the real home (no env
//!      override exists — we deliberately invent none).
//!
//! Until every line is confirmed, treat the `FRESH_INSTALL` fixture bytes and [`PLUGIN_CONTENT`] as
//! provisional.

use std::path::PathBuf;

use serde_json::{Map, Value};
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::{ConfigStore, DiscoveredPlacement, HarnessAdapter, PlacementNaming, PlacementTarget};

/// The user-scope layer label recorded for a discovered/placed OpenClaw skill (the resolved layer;
/// a project/enterprise layer stays representable later — `DiscoveredPlacement.layer` is already
/// `Option<String>`).
const LAYER_USER: &str = "user";

/// The structured marker identity reported in [`TriggerReport::marker_id`] — and the in-file marker
/// that makes the plugin file OURS: a file at the plugin path *without* this marker is foreign and
/// is never registered, never overwritten, never unlinked (adopt-or-leave, mirroring the reference's
/// sentinel + command-identity double guard).
const MARKER_ID: &str = "topos:openclaw:currency:1";

/// The topos-owned inject plugin's file name, flat under the OpenClaw home (deliberately NOT a
/// `plugins/` subdir: a clean scrub then leaves zero residue — no emptied directory — and the file
/// never sits in a directory a build might auto-scan). The registration entry is this file's
/// absolute path, which doubles as the managed-entry identity.
const PLUGIN_FILE_NAME: &str = "topos-currency.mjs";

/// The provisional `openclaw.json` key holding the bootstrap-inject registrations (an array of file
/// paths injected at gateway bootstrap). Probe item 5.
const EXTRA_FILES_KEY: &str = "bootstrap-extra-files";

/// The provisional `openclaw.json` flag disabling the bootstrap-inject surface. Absent = enabled
/// (probe item 1 re-confirms that default); `false` — or any non-bool — degrades honestly, because
/// a registration into a disabled surface would claim currency it cannot deliver.
const INJECT_FLAG_KEY: &str = "bootstrap-inject";

/// The provisional injected-context char budget (probe item 4). A build property, not a config key —
/// the planner takes the budget as a parameter so the blown-budget degrade is genuinely exercised in
/// tests while production passes this probe-pinned ceiling.
const INJECT_CHAR_BUDGET: usize = 4096;

/// The inert inject plugin topos installs — a pure data default-export with zero side effects
/// whether or not a gateway loads it. The per-`topos`-touch refresh of this surface's CONTENTS is
/// the sync engine's job, not this adapter's, and is **not yet wired** — until it lands, the
/// installed surface keeps exactly these bytes, which claim no update (`lastRefreshed: null`) and
/// point at `topos update`, so it never announces an update it has not seen. Install checks only
/// marker-ownership, never byte-equality, so a refreshed file is still a true no-op re-install.
const PLUGIN_CONTENT: &str = "\
// topos:openclaw:currency:1 — the topos-managed bootstrap-inject surface.
// Managed by topos (topos.sh): topos rewrites this file's contents on each `topos` touch, so
// hand-edits are overwritten. Remove it with `topos uninstall` (which also scrubs the
// registration in openclaw.json), not by hand.
//
// This surface shows its LAST-REFRESHED state: skill updates surface on the
// first `topos` touch of a session — an honest one-beat-in latency — never at
// bare session open, and never via cron. To refresh now, run: topos update
export default {
  topos: \"currency\",
  version: 1,
  lastRefreshed: null,
  updates: [],
};
";

/// The `OpenClaw` [`HarnessAdapter`]. Holds the resolved config home (injected, so tests point it at
/// a temp dir) and the [`ConfigStore`] port that performs the durable config writes.
pub struct OpenClaw<'a> {
    /// `$HOME/.openclaw` — injected in tests; see [`OpenClaw::resolve_home`].
    home: PathBuf,
    cfg: &'a dyn ConfigStore,
}

impl std::fmt::Debug for OpenClaw<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenClaw")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl<'a> OpenClaw<'a> {
    /// Construct over an explicit config home + a config-store port. Production passes
    /// [`OpenClaw::resolve_home`]; tests pass a temp dir so a real `~/.openclaw` is never touched.
    #[must_use]
    pub fn new(home: PathBuf, cfg: &'a dyn ConfigStore) -> Self {
        Self { home, cfg }
    }

    /// Resolve OpenClaw's config home: `$HOME/.openclaw` (falling back to `./.openclaw` if `$HOME`
    /// is unset). Deliberately NO env-var override — the pilot build defines none we can trust yet
    /// (a probe item; inventing one would be a fake probe result).
    #[must_use]
    pub fn resolve_home() -> PathBuf {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".openclaw")
    }

    fn skills_dir(&self) -> PathBuf {
        self.home.join("skills")
    }

    fn config_path(&self) -> PathBuf {
        self.home.join("openclaw.json")
    }

    fn plugin_path(&self) -> PathBuf {
        self.home.join(PLUGIN_FILE_NAME)
    }

    /// The canonical registration entry — the plugin file's absolute path as the config records it.
    fn canonical_entry(&self) -> String {
        self.plugin_path().to_string_lossy().into_owned()
    }

    /// Read the current config bytes, `None` if absent, `Err` only on a genuine I/O failure.
    fn read_config(&self) -> std::io::Result<Option<Vec<u8>>> {
        self.cfg.read(&self.config_path())
    }

    /// Probe the plugin path through the port: absent, ours (bytes carry [`MARKER_ID`]), or a
    /// foreign file squatting on our path (never registered / overwritten / unlinked).
    fn plugin_file(&self) -> std::io::Result<PluginFile> {
        Ok(match self.cfg.read(&self.plugin_path())? {
            None => PluginFile::Absent,
            Some(bytes) if String::from_utf8_lossy(&bytes).contains(MARKER_ID) => PluginFile::Ours,
            Some(_) => PluginFile::Foreign,
        })
    }

    /// Apply a planned edit, degrading honestly on any write failure — never a blind overwrite.
    fn apply(&self, plan: EditPlan) -> TriggerReport {
        match plan {
            EditPlan::Leave(state) => self.report(state, None),
            EditPlan::Install {
                write_plugin,
                config,
                state,
            } => {
                // Plugin file first, then the registration: the config entry is the sole commit
                // point, so a crash between the two leaves an inert unregistered file (healed by a
                // re-run), never a registration pointing at nothing.
                if write_plugin
                    && self
                        .cfg
                        .replace(&self.plugin_path(), PLUGIN_CONTENT.as_bytes())
                        .is_err()
                {
                    return self.report(TriggerState::Degraded, None);
                }
                match config {
                    None => self.report(state, write_plugin.then(|| self.plugin_path())),
                    Some(bytes) => match self.cfg.replace(&self.config_path(), &bytes) {
                        Ok(()) => self.report(state, Some(self.config_path())),
                        // The plugin write (if any) already durably landed — disclose it even
                        // though the registration failed; the file is inert while unregistered.
                        Err(_) => self.report(
                            TriggerState::Degraded,
                            write_plugin.then(|| self.plugin_path()),
                        ),
                    },
                }
            }
            EditPlan::Scrub {
                config,
                unlink,
                state,
            } => {
                let mut touched = None;
                if let Some(bytes) = config {
                    // De-reference before delete: scrub the registration first, so the config
                    // never points at a removed file.
                    if self.cfg.replace(&self.config_path(), &bytes).is_err() {
                        return self.report(TriggerState::Degraded, None);
                    }
                    touched = Some(self.config_path());
                }
                if unlink {
                    // The port has no delete (the trait is frozen), so the one direct fs call: a
                    // best-effort unlink of OUR OWN marker-confirmed file, only ever after it is
                    // de-referenced. A failure leaves an inert orphan the footprint still
                    // discloses — the trigger itself is cleanly off, so the state stands.
                    match std::fs::remove_file(self.plugin_path()) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => {}
                    }
                }
                self.report(state, touched)
            }
        }
    }

    fn report(&self, state: TriggerState, touched: Option<PathBuf>) -> TriggerReport {
        TriggerReport {
            harness: HarnessId::OpenClaw,
            // Honest labeling: FirstToposTouch only for a verified, active trigger we own; every
            // other state advertises just the guaranteed floor — an explicit `topos update`.
            currency_kind: if state == TriggerState::Active {
                CurrencyKind::FirstToposTouch
            } else {
                CurrencyKind::ExplicitPullOnly
            },
            touched_path: touched.map(|p| p.to_string_lossy().into_owned()),
            marker_id: MARKER_ID.to_owned(),
            state,
        }
    }

    /// Whether the managed registration entry is currently present (drives `--footprint`
    /// disclosure). A missing/unreadable/malformed config means "not present" — we never claim to
    /// own a path we cannot confirm.
    fn has_managed_entry(&self) -> bool {
        let Ok(Some(bytes)) = self.read_config() else {
            return false;
        };
        let Ok(root) = serde_json::from_slice::<Value>(&bytes) else {
            return false;
        };
        matches!(
            extra_files_ref(&root).map(|e| classify(e, &self.canonical_entry())),
            Some(Classification::Managed)
        )
    }
}

impl HarnessAdapter for OpenClaw<'_> {
    fn id(&self) -> HarnessId {
        HarnessId::OpenClaw
    }

    fn discover(&self) -> Vec<DiscoveredPlacement> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.skills_dir()) else {
            return out; // no skills dir (or unreadable) → nothing discovered, never an error
        };
        for entry in entries.flatten() {
            // The skill name is the directory name, so a non-UTF-8 name can't be a skill we manage.
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
            // frontmatter (all-optional, and we never parse it), so a malformed SKILL.md can't
            // mislead.
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
        // Name-based placement is the reference (Claude Code) adapter's; this pilot adapter's concrete
        // dir shape stays id-keyed until its readiness probe, so the display name is not used here yet.
        _naming: PlacementNaming<'_>,
        discovered: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget {
        match discovered {
            Some(d) => PlacementTarget {
                dir: d.path.clone(),
            },
            // No-discovered default: `<home>/skills/<skill_id>` — the resolved user layer.
            None => PlacementTarget {
                dir: self.skills_dir().join(skill_id),
            },
        }
    }

    fn currency_kind(&self) -> CurrencyKind {
        CurrencyKind::FirstToposTouch
    }

    fn install_currency_trigger(&self) -> TriggerReport {
        let Ok(plugin) = self.plugin_file() else {
            // The plugin path is unreadable (not merely absent) — degrade, write nothing.
            return self.report(TriggerState::Degraded, None);
        };
        match self.read_config() {
            Ok(current) => self.apply(plan_install(
                current.as_deref(),
                plugin,
                &self.canonical_entry(),
                INJECT_CHAR_BUDGET,
            )),
            // Unreadable (e.g. a permission error) — degrade honestly, never blind-overwrite.
            Err(_) => self.report(TriggerState::Degraded, None),
        }
    }

    fn remove_currency_trigger(&self) -> TriggerReport {
        let Ok(plugin) = self.plugin_file() else {
            return self.report(TriggerState::Degraded, None);
        };
        match self.read_config() {
            Ok(current) => self.apply(plan_remove(
                current.as_deref(),
                plugin,
                &self.canonical_entry(),
            )),
            Err(_) => self.report(TriggerState::Degraded, None),
        }
    }

    fn uninstall_footprint(&self) -> Vec<PathBuf> {
        // Disclosure-only, never delete targets for the shared config (it is scrubbed via
        // `remove_currency_trigger`, the file kept). The plugin path is disclosed on
        // marker-confirmed existence — so a de-referenced orphan (a crash window, or a failed
        // unlink) is still disclosed honestly, while a foreign file at our path is not claimed.
        let mut out = Vec::new();
        if self.has_managed_entry() {
            out.push(self.config_path());
        }
        if matches!(self.plugin_file(), Ok(PluginFile::Ours)) {
            out.push(self.plugin_path());
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// The pure openclaw.json merge — bytes in → an edit plan out. No I/O; fail-closed on anything we
// cannot safely interpret (never coerce or clobber a user's differently-shaped config).
// ---------------------------------------------------------------------------------------------

/// What the plugin path currently holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginFile {
    Absent,
    /// A file carrying [`MARKER_ID`] — ours to (re)use, heal, and unlink.
    Ours,
    /// A file WITHOUT our marker squatting on our path — never registered, overwritten, or
    /// unlinked (registering unknown bytes into an LLM-context surface is the one thing this
    /// adapter must never do).
    Foreign,
}

/// What a planned edit does. `Install` writes the plugin file (when needed) and then the config
/// post-image; `Scrub` writes the scrubbed config and then unlinks our plugin file; `Leave` touches
/// nothing (a true no-op — an unchanged re-run never re-serializes the user's file).
enum EditPlan {
    Leave(TriggerState),
    Install {
        write_plugin: bool,
        config: Option<Vec<u8>>,
        state: TriggerState,
    },
    Scrub {
        config: Option<Vec<u8>>,
        unlink: bool,
        state: TriggerState,
    },
}

/// How the existing registrations relate to topos's managed entry.
#[derive(Debug, PartialEq, Eq)]
enum Classification {
    /// Our exact canonical entry is present.
    Managed,
    /// A topos-currency-shaped entry exists that is NOT our canonical one (hand-rolled, or an old
    /// home's path) — adopt-or-leave, never blind-append and never scrub.
    Unmanaged,
    /// No topos currency registration at all.
    Absent,
}

fn plan_install(
    current: Option<&[u8]>,
    plugin: PluginFile,
    canonical: &str,
    budget: usize,
) -> EditPlan {
    let mut root = match parse_config(current) {
        ParsedConfig::Fresh => Value::Object(Map::new()),
        ParsedConfig::Value(v) => v,
        ParsedConfig::Malformed => return EditPlan::Leave(TriggerState::Degraded),
    };
    let Some(obj) = root.as_object_mut() else {
        return EditPlan::Leave(TriggerState::Degraded); // valid JSON, non-object root — fail closed
    };
    // The capacity gates run BEFORE classification: a managed entry under a disabled inject
    // surface (or a blown budget) honestly reports Degraded — currency cannot fire — with no write.
    match obj.get(INJECT_FLAG_KEY) {
        None | Some(Value::Bool(true)) => {}
        Some(_) => return EditPlan::Leave(TriggerState::Degraded), // disabled or wrong-typed
    }
    if PLUGIN_CONTENT.chars().count() > budget {
        return EditPlan::Leave(TriggerState::Degraded); // blown char budget — degrade, never truncate
    }
    let entries: &[Value] = match obj.get(EXTRA_FILES_KEY) {
        None => &[],
        Some(Value::Array(a)) => a,
        Some(_) => return EditPlan::Leave(TriggerState::Degraded), // un-appliable fresh-array
    };
    match classify(entries, canonical) {
        Classification::Managed => match plugin {
            PluginFile::Ours => EditPlan::Leave(TriggerState::Active), // already ours → true no-op
            // Registered but the file vanished (a hand-delete): heal the file, config untouched.
            PluginFile::Absent => EditPlan::Install {
                write_plugin: true,
                config: None,
                state: TriggerState::Active,
            },
            // Our registration, someone else's bytes — leave both; never vouch, never clobber.
            PluginFile::Foreign => EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged),
        },
        Classification::Unmanaged => EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged),
        Classification::Absent => {
            if plugin == PluginFile::Foreign {
                // A foreign file squats on our path with NOTHING registered: registering it would
                // inject unknown bytes into LLM context, and overwriting it would clobber someone's
                // file — but no equivalent trigger exists either, so `AlreadyPresentUnmanaged`
                // would be a false claim. A path collision is a Degraded install: nothing armed,
                // nothing written, the explicit-pull floor advertised.
                return EditPlan::Leave(TriggerState::Degraded);
            }
            // The openclaw#51789-safe fresh-array form: build a brand-new array and assign it
            // wholesale — never push into the live one.
            let mut fresh: Vec<Value> = entries.to_vec();
            fresh.push(Value::String(canonical.to_owned()));
            obj.insert(EXTRA_FILES_KEY.to_owned(), Value::Array(fresh));
            match serialize(&root) {
                Some(bytes) => EditPlan::Install {
                    write_plugin: plugin == PluginFile::Absent,
                    config: Some(bytes),
                    state: TriggerState::Active,
                },
                None => EditPlan::Leave(TriggerState::Degraded),
            }
        }
    }
}

fn plan_remove(current: Option<&[u8]>, plugin: PluginFile, canonical: &str) -> EditPlan {
    let mut root = match parse_config(current) {
        // No config at all: nothing registered; unlink a confirmed orphan of OURS if one exists.
        ParsedConfig::Fresh => {
            return EditPlan::Scrub {
                config: None,
                unlink: plugin == PluginFile::Ours,
                state: TriggerState::Inactive,
            };
        }
        ParsedConfig::Value(v) => v,
        // Present but unreadable: never touch the config, and never unlink the plugin either — a
        // format we can't parse may still reference it in a shape we don't understand.
        ParsedConfig::Malformed => return EditPlan::Leave(TriggerState::Degraded),
    };
    let Some(obj) = root.as_object_mut() else {
        return EditPlan::Leave(TriggerState::Degraded);
    };
    let entries: &[Value] = match obj.get(EXTRA_FILES_KEY) {
        None => &[],
        Some(Value::Array(a)) => a,
        // Wrong-typed while the real format is provisional: the managed path could still be live
        // inside a shape we don't understand — degrade, no scrub, no unlink.
        Some(_) => return EditPlan::Leave(TriggerState::Degraded),
    };
    match classify(entries, canonical) {
        Classification::Absent => EditPlan::Scrub {
            config: None,
            unlink: plugin == PluginFile::Ours,
            state: TriggerState::Inactive,
        },
        Classification::Unmanaged => EditPlan::Leave(TriggerState::AlreadyPresentUnmanaged),
        Classification::Managed => {
            // Fresh-array scrub of ONLY our exact entry; prune the key when OUR removal emptied it
            // (restoring toward the pre-install shape — accepting, like the reference, the edge
            // where a user's own pre-existing empty array is pruned too).
            let fresh: Vec<Value> = entries
                .iter()
                .filter(|e| e.as_str() != Some(canonical))
                .cloned()
                .collect();
            if fresh.is_empty() {
                obj.remove(EXTRA_FILES_KEY);
            } else {
                obj.insert(EXTRA_FILES_KEY.to_owned(), Value::Array(fresh));
            }
            match serialize(&root) {
                Some(bytes) => EditPlan::Scrub {
                    config: Some(bytes),
                    unlink: plugin == PluginFile::Ours,
                    state: TriggerState::Inactive,
                },
                None => EditPlan::Leave(TriggerState::Degraded),
            }
        }
    }
}

/// The parse outcome for the existing config bytes.
enum ParsedConfig {
    /// Absent or whitespace-only — start from a fresh object.
    Fresh,
    /// Parsed JSON.
    Value(Value),
    /// Present but not valid JSON — fail closed (never clobber a file we can't read).
    Malformed,
}

fn parse_config(current: Option<&[u8]>) -> ParsedConfig {
    match current {
        None => ParsedConfig::Fresh,
        Some(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => ParsedConfig::Fresh,
        Some(bytes) => match serde_json::from_slice::<Value>(bytes) {
            Ok(value) => ParsedConfig::Value(value),
            Err(_) => ParsedConfig::Malformed,
        },
    }
}

/// The extra-files array as a shared slice IF it exists and is well-typed.
fn extra_files_ref(root: &Value) -> Option<&Vec<Value>> {
    root.as_object()?.get(EXTRA_FILES_KEY)?.as_array()
}

/// Classify the existing registrations against topos's canonical entry. Ours iff an entry is the
/// exact canonical plugin path; a topos-currency-shaped entry anywhere else (our file name at a
/// foreign location, or a name carrying both "topos" and "currency") is a hand-rolled equivalent we
/// adopt-or-leave; anything else — including a near-miss bearing only one of the tokens — is not a
/// currency registration at all.
fn classify(entries: &[Value], canonical: &str) -> Classification {
    let mut unmanaged = false;
    for entry in entries {
        let Some(path) = entry.as_str() else {
            continue; // a non-string entry is not a shape we interpret
        };
        if path == canonical {
            return Classification::Managed;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        if name == PLUGIN_FILE_NAME || (name.contains("topos") && name.contains("currency")) {
            unmanaged = true;
        }
    }
    if unmanaged {
        Classification::Unmanaged
    } else {
        Classification::Absent
    }
}

/// Serialize the merged config: 2-space pretty + a trailing newline, keys alphabetical
/// (`serde_json`'s default — we deliberately do NOT enable `preserve_order`, a workspace-global
/// feature flip). Provisional: probe item 5 re-confirms this matches OpenClaw's own writer. A write
/// happens only on a real change, so any normalization is one-time and action-triggered.
fn serialize(root: &Value) -> Option<Vec<u8>> {
    let mut text = serde_json::to_string_pretty(root).ok()?;
    text.push('\n');
    Some(text.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A path-keyed in-memory [`ConfigStore`] (the adapter writes TWO files — the config and the
    /// plugin — so the double must key by path). For the pure-merge tests; the unlink path needs
    /// [`DiskConfig`] because `remove_currency_trigger` deletes with `std::fs::remove_file`.
    #[derive(Debug, Default)]
    struct MemConfig {
        files: RefCell<HashMap<PathBuf, Vec<u8>>>,
        writes: RefCell<u32>,
    }
    impl MemConfig {
        fn with_config(bytes: &str) -> Self {
            let me = Self::default();
            me.files
                .borrow_mut()
                .insert(PathBuf::from("/h/openclaw.json"), bytes.as_bytes().to_vec());
            me
        }
        fn seed(&self, path: &str, bytes: &[u8]) {
            self.files
                .borrow_mut()
                .insert(PathBuf::from(path), bytes.to_vec());
        }
        fn text(&self, path: &str) -> Option<String> {
            self.files
                .borrow()
                .get(Path::new(path))
                .map(|b| String::from_utf8(b.clone()).unwrap())
        }
        fn writes(&self) -> u32 {
            *self.writes.borrow()
        }
    }
    impl ConfigStore for MemConfig {
        fn read(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
            Ok(self.files.borrow().get(path).cloned())
        }
        fn replace(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            self.files
                .borrow_mut()
                .insert(path.to_path_buf(), bytes.to_vec());
            *self.writes.borrow_mut() += 1;
            Ok(())
        }
    }

    /// A real-disk [`ConfigStore`] over a temp home, for the tests where the `std::fs` unlink (and
    /// `discover`'s `read_dir`) must be observable — an in-memory double would mask a false green.
    #[derive(Debug)]
    struct DiskConfig;
    impl ConfigStore for DiskConfig {
        fn read(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
            match std::fs::read(path) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }
        fn replace(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, bytes)
        }
    }

    /// A self-cleaning temp dir (RAII).
    struct TempHome(PathBuf);
    impl TempHome {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("topos-oc-{}-{n}", std::process::id()));
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

    fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> OpenClaw<'a> {
        OpenClaw::new(home.to_path_buf(), cfg)
    }

    const CONFIG: &str = "/h/openclaw.json";
    const PLUGIN: &str = "/h/topos-currency.mjs";

    /// The exact bytes a fresh install writes into an absent `openclaw.json` — the byte-compared
    /// fixture (2-space pretty, trailing newline). PROVISIONAL: pinned at the readiness probe.
    const FRESH_INSTALL: &str = "\
{
  \"bootstrap-extra-files\": [
    \"/h/topos-currency.mjs\"
  ]
}
";

    #[test]
    fn install_into_absent_config_writes_the_plugin_and_the_exact_registration() {
        let cfg = MemConfig::default(); // both files absent
        let report = adapter(Path::new("/h"), &cfg).install_currency_trigger();

        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.harness, HarnessId::OpenClaw);
        assert_eq!(report.currency_kind, CurrencyKind::FirstToposTouch);
        assert_eq!(report.marker_id, MARKER_ID);
        assert_eq!(
            report.touched_path.as_deref(),
            Some(CONFIG),
            "the registration is the commit point"
        );
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(FRESH_INSTALL));
        assert_eq!(cfg.text(PLUGIN).as_deref(), Some(PLUGIN_CONTENT));
        assert_eq!(cfg.writes(), 2, "one plugin write + one config write");
    }

    #[test]
    fn install_is_idempotent_a_true_no_op_on_rerun() {
        let cfg = MemConfig::default();
        let home = Path::new("/h");
        adapter(home, &cfg).install_currency_trigger();

        let report = adapter(home, &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert!(
            report.touched_path.is_none(),
            "idempotent re-run touches nothing"
        );
        assert_eq!(cfg.writes(), 2, "re-run writes to NEITHER file");
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(FRESH_INSTALL));
    }

    #[test]
    fn install_preserves_foreign_keys_and_sibling_registrations() {
        let cfg = MemConfig::with_config(
            "{\n  \"model\": \"opus\",\n  \"bootstrap-extra-files\": [\"/h/notes.md\"]\n}\n",
        );
        let report = adapter(Path::new("/h"), &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);

        let root: Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert_eq!(root["model"], "opus", "foreign top-level key survives");
        let entries = root[EXTRA_FILES_KEY].as_array().unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|e| e.as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["/h/notes.md", PLUGIN],
            "the fresh array keeps the sibling and appends ours"
        );
    }

    #[test]
    fn install_heals_a_missing_plugin_file_without_touching_the_config() {
        let cfg = MemConfig::default();
        let home = Path::new("/h");
        adapter(home, &cfg).install_currency_trigger();
        cfg.files.borrow_mut().remove(Path::new(PLUGIN)); // a hand-delete

        let report = adapter(home, &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(
            report.touched_path.as_deref(),
            Some(PLUGIN),
            "only the plugin file is healed"
        );
        assert_eq!(cfg.text(PLUGIN).as_deref(), Some(PLUGIN_CONTENT));
        assert_eq!(cfg.writes(), 3, "the config was NOT re-written");
    }

    #[test]
    fn install_never_registers_or_overwrites_a_foreign_file_on_our_path() {
        // A file without our marker squats on the plugin path and NOTHING is registered: a path
        // collision — no trigger exists, so the install degrades (never `AlreadyPresentUnmanaged`,
        // which would falsely claim an equivalent trigger); neither file is written.
        let cfg = MemConfig::default();
        cfg.seed(PLUGIN, b"export default { evil: true };\n");
        let report = adapter(Path::new("/h"), &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Degraded);
        assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
        assert_eq!(cfg.writes(), 0, "neither file is written");
        assert!(cfg.text(CONFIG).is_none(), "no registration was created");

        // When OUR registration exists but the file's bytes lost the marker (a hand-edit), a live
        // inject surface really is present without our marker — adopt-or-leave, never clobber.
        let cfg = MemConfig::with_config(FRESH_INSTALL);
        cfg.seed(PLUGIN, b"export default { edited: true };\n");
        let report = adapter(Path::new("/h"), &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0, "never vouch, never clobber");
    }

    #[test]
    fn non_string_sibling_entries_survive_install_and_remove_untouched() {
        // The fresh-array clone must carry every sibling VERBATIM — including entries that are not
        // strings (a number, an object) — on both the append and the scrub.
        let cfg = MemConfig::with_config(
            "{\"bootstrap-extra-files\": [42, {\"path\": \"x\"}, \"/h/notes.md\"]}",
        );
        let home = Path::new("/h");
        let report = adapter(home, &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        let root: Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert_eq!(
            root[EXTRA_FILES_KEY],
            serde_json::json!([42, {"path": "x"}, "/h/notes.md", PLUGIN]),
            "non-string siblings survive in order; ours is appended once"
        );

        let report = adapter(home, &cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        let root: Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert_eq!(
            root[EXTRA_FILES_KEY],
            serde_json::json!([42, {"path": "x"}, "/h/notes.md"]),
            "the scrub removes only ours, keeping every non-string sibling"
        );
    }

    #[test]
    fn a_double_listed_canonical_entry_is_a_no_op_install_and_a_full_scrub() {
        // Managed wins on the first exact match, so install never appends a third copy; the scrub
        // filter drops EVERY copy of the exact canonical entry and prunes the emptied key.
        let cfg = MemConfig::with_config(
            "{\"bootstrap-extra-files\": [\"/h/topos-currency.mjs\", \"/h/topos-currency.mjs\"]}",
        );
        cfg.seed(PLUGIN, PLUGIN_CONTENT.as_bytes());
        let home = Path::new("/h");
        let report = adapter(home, &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(
            cfg.writes(),
            0,
            "a double-listed managed entry is still a no-op install"
        );

        let report = adapter(home, &cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        let root: Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert!(
            root.get(EXTRA_FILES_KEY).is_none(),
            "both copies are scrubbed and the emptied key pruned"
        );
    }

    #[test]
    fn install_leaves_a_hand_rolled_currency_registration_unmanaged() {
        // Our file name at a foreign location, and a topos+currency-named file: both adopt-or-leave.
        for entry in [
            "/elsewhere/topos-currency.mjs",
            "/x/my-topos-currency-note.md",
        ] {
            let cfg =
                MemConfig::with_config(&format!("{{\"bootstrap-extra-files\": [\"{entry}\"]}}"));
            let report = adapter(Path::new("/h"), &cfg).install_currency_trigger();
            assert_eq!(
                report.state,
                TriggerState::AlreadyPresentUnmanaged,
                "{entry} is a hand-rolled equivalent"
            );
            assert_eq!(cfg.writes(), 0);
        }
    }

    #[test]
    fn a_stray_near_miss_entry_is_not_claimed_or_blocked() {
        // Only one token ("topos" without "currency") → not a currency registration; install ours.
        let cfg =
            MemConfig::with_config("{\"bootstrap-extra-files\": [\"/h/topos-skills-index.mjs\"]}");
        let report = adapter(Path::new("/h"), &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        let root: Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert_eq!(
            root[EXTRA_FILES_KEY].as_array().unwrap().len(),
            2,
            "the near-miss is kept AND ours is appended"
        );
    }

    #[test]
    fn install_fails_closed_on_malformed_or_wrong_typed_config() {
        // Malformed JSON → degrade, no write, bytes untouched.
        let bad = MemConfig::with_config("{ this is not json ");
        let r = adapter(Path::new("/h"), &bad).install_currency_trigger();
        assert_eq!(r.state, TriggerState::Degraded);
        assert_eq!(r.currency_kind, CurrencyKind::ExplicitPullOnly);
        assert_eq!(bad.writes(), 0);
        assert_eq!(bad.text(CONFIG).as_deref(), Some("{ this is not json "));

        // The extra-files key present but wrong-typed → the fresh-array is un-appliable.
        let wrong = MemConfig::with_config("{\"bootstrap-extra-files\": \"oops\"}");
        let r = adapter(Path::new("/h"), &wrong).install_currency_trigger();
        assert_eq!(r.state, TriggerState::Degraded);
        assert_eq!(wrong.writes(), 0);

        // A non-object root → fail closed.
        let arr = MemConfig::with_config("[1, 2]");
        let r = adapter(Path::new("/h"), &arr).install_currency_trigger();
        assert_eq!(r.state, TriggerState::Degraded);
        assert_eq!(arr.writes(), 0);
    }

    #[test]
    fn install_degrades_when_bootstrap_inject_is_disabled_or_wrong_typed() {
        for config in [
            "{\"bootstrap-inject\": false}",
            "{\"bootstrap-inject\": \"yes\"}",
        ] {
            let cfg = MemConfig::with_config(config);
            let report = adapter(Path::new("/h"), &cfg).install_currency_trigger();
            assert_eq!(report.state, TriggerState::Degraded, "for {config}");
            assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
            assert_eq!(cfg.writes(), 0, "a disabled surface is never written into");
        }
        // The gate outranks classification: even OUR OWN entry reports Degraded while disabled.
        let cfg = MemConfig::with_config(
            "{\"bootstrap-extra-files\": [\"/h/topos-currency.mjs\"], \"bootstrap-inject\": false}",
        );
        cfg.seed(PLUGIN, PLUGIN_CONTENT.as_bytes());
        let report = adapter(Path::new("/h"), &cfg).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Degraded);
        assert_eq!(cfg.writes(), 0);
    }

    #[test]
    fn a_blown_char_budget_degrades_without_writing() {
        // The planner takes the budget as a parameter (production passes the probe-pinned const),
        // so the blown row is genuinely exercised: a ceiling smaller than our content degrades.
        let plan = plan_install(None, PluginFile::Absent, "/h/topos-currency.mjs", 16);
        assert!(matches!(plan, EditPlan::Leave(TriggerState::Degraded)));
        // A generous ceiling installs.
        let plan = plan_install(None, PluginFile::Absent, "/h/topos-currency.mjs", 1 << 20);
        assert!(matches!(
            plan,
            EditPlan::Install {
                write_plugin: true,
                config: Some(_),
                state: TriggerState::Active,
            }
        ));
        // The production const itself holds our content (the fit is pinned, not assumed).
        assert!(PLUGIN_CONTENT.chars().count() <= INJECT_CHAR_BUDGET);
    }

    #[test]
    fn remove_scrubs_the_entry_unlinks_the_plugin_and_keeps_siblings_then_is_idempotent() {
        let home = TempHome::new();
        let cfg = DiskConfig;
        std::fs::write(
            home.0.join("openclaw.json"),
            "{\n  \"model\": \"opus\",\n  \"bootstrap-extra-files\": [\"/h/notes.md\"]\n}\n",
        )
        .unwrap();
        let a = adapter(&home.0, &cfg);
        let installed = a.install_currency_trigger();
        assert_eq!(installed.state, TriggerState::Active);
        assert!(home.0.join(PLUGIN_FILE_NAME).is_file());

        let report = a.remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
        assert!(
            !home.0.join(PLUGIN_FILE_NAME).exists(),
            "our plugin file was unlinked from real disk"
        );
        let root: Value =
            serde_json::from_slice(&std::fs::read(home.0.join("openclaw.json")).unwrap()).unwrap();
        assert_eq!(root["model"], "opus", "foreign key survives the scrub");
        assert_eq!(
            root[EXTRA_FILES_KEY].as_array().unwrap().len(),
            1,
            "the sibling registration survives; only ours is scrubbed"
        );

        // Idempotent: a second remove is a clean no-op.
        let report = a.remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(report.touched_path.is_none());
    }

    #[test]
    fn remove_prunes_the_key_our_removal_emptied_restoring_the_pre_install_shape() {
        let cfg = MemConfig::default();
        let home = Path::new("/h");
        adapter(home, &cfg).install_currency_trigger();
        let report = adapter(home, &cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        let root: Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert!(
            root.get(EXTRA_FILES_KEY).is_none(),
            "the key we created was pruned away"
        );
    }

    #[test]
    fn remove_leaves_a_hand_rolled_registration_and_an_absent_config_alone() {
        let cfg = MemConfig::with_config(
            "{\"bootstrap-extra-files\": [\"/elsewhere/topos-currency.mjs\"]}",
        );
        let report = adapter(Path::new("/h"), &cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0);

        // An absent config → a clean no-op, never created.
        let absent = MemConfig::default();
        let report = adapter(Path::new("/h"), &absent).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(
            absent.text(CONFIG).is_none(),
            "remove never creates the file"
        );
    }

    #[test]
    fn remove_degrades_on_malformed_or_wrong_typed_without_clobbering_or_unlinking() {
        // Malformed config + our marked plugin on real disk: neither is touched — a format we
        // can't parse may still reference the file in a shape we don't understand.
        let home = TempHome::new();
        let cfg = DiskConfig;
        std::fs::write(home.0.join("openclaw.json"), "{ not json ").unwrap();
        std::fs::write(home.0.join(PLUGIN_FILE_NAME), PLUGIN_CONTENT).unwrap();
        let report = adapter(&home.0, &cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Degraded);
        assert_eq!(
            std::fs::read_to_string(home.0.join("openclaw.json")).unwrap(),
            "{ not json ",
            "never clobbered"
        );
        assert!(
            home.0.join(PLUGIN_FILE_NAME).is_file(),
            "the plugin is never unlinked under a config we can't read"
        );

        // Wrong-typed extra-files while the real format is provisional: same posture.
        std::fs::write(
            home.0.join("openclaw.json"),
            "{\"bootstrap-extra-files\": \"oops\"}",
        )
        .unwrap();
        let report = adapter(&home.0, &cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Degraded);
        assert!(home.0.join(PLUGIN_FILE_NAME).is_file());
    }

    #[test]
    fn remove_unlinks_a_confirmed_orphan_but_never_a_foreign_file() {
        // Our marked file with no registration (a crash window's leftover): confirmed ours → unlink.
        let home = TempHome::new();
        let cfg = DiskConfig;
        std::fs::write(home.0.join(PLUGIN_FILE_NAME), PLUGIN_CONTENT).unwrap();
        let report = adapter(&home.0, &cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(
            !home.0.join(PLUGIN_FILE_NAME).exists(),
            "the confirmed orphan was unlinked"
        );

        // A foreign (marker-less) file on our path: never ours to delete.
        std::fs::write(home.0.join(PLUGIN_FILE_NAME), "export default {};\n").unwrap();
        let report = adapter(&home.0, &cfg).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(
            home.0.join(PLUGIN_FILE_NAME).is_file(),
            "a foreign file is left in place"
        );
    }

    #[test]
    fn footprint_disclosed_only_for_the_managed_entry_and_our_own_plugin() {
        let home = TempHome::new();
        let cfg = DiskConfig;
        let a = adapter(&home.0, &cfg);
        assert!(
            a.uninstall_footprint().is_empty(),
            "nothing installed → nothing disclosed"
        );

        a.install_currency_trigger();
        assert_eq!(
            a.uninstall_footprint(),
            vec![home.0.join("openclaw.json"), home.0.join(PLUGIN_FILE_NAME)],
            "both owned paths disclosed (never as delete targets for the shared config)"
        );

        a.remove_currency_trigger();
        assert!(
            a.uninstall_footprint().is_empty(),
            "a clean scrub leaves no footprint"
        );

        // A de-referenced orphan of OURS is still disclosed (file-existence-keyed)…
        std::fs::write(home.0.join(PLUGIN_FILE_NAME), PLUGIN_CONTENT).unwrap();
        assert_eq!(a.uninstall_footprint(), vec![home.0.join(PLUGIN_FILE_NAME)]);
        // …but a foreign file on our path is never claimed.
        std::fs::write(home.0.join(PLUGIN_FILE_NAME), "export default {};\n").unwrap();
        assert!(a.uninstall_footprint().is_empty());
    }

    #[test]
    fn reports_label_first_topos_touch_only_when_active_and_never_session_start() {
        let cfg = MemConfig::default();
        let a = adapter(Path::new("/h"), &cfg);
        assert_eq!(a.currency_kind(), CurrencyKind::FirstToposTouch);

        let active = a.install_currency_trigger();
        assert_eq!(active.currency_kind, CurrencyKind::FirstToposTouch);
        let inactive = a.remove_currency_trigger();
        assert_eq!(
            inactive.currency_kind,
            CurrencyKind::ExplicitPullOnly,
            "anything but Active advertises only the guaranteed floor"
        );
        assert_ne!(active.currency_kind, CurrencyKind::SessionStart);
        assert!(
            !PLUGIN_CONTENT.to_lowercase().contains("session start"),
            "the inject surface never claims session-start currency"
        );
    }

    #[test]
    fn discover_finds_skill_dirs_and_ignores_non_skills_without_panic() {
        let home = TempHome::new();
        home.skill("pr-describe");
        home.skill("commit-msg");
        // A dir with no SKILL.md is not a skill; a dot-dir and a stray file are skipped.
        std::fs::create_dir_all(home.0.join("skills").join("not-a-skill")).unwrap();
        std::fs::create_dir_all(home.0.join("skills").join(".topos-staging-x")).unwrap();
        std::fs::write(
            home.0
                .join("skills")
                .join(".topos-staging-x")
                .join("SKILL.md"),
            b"x",
        )
        .unwrap();
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
        let found = adapter(Path::new("/no-such-openclaw-home-xyz"), &cfg).discover();
        assert!(found.is_empty());
    }

    #[test]
    fn placement_for_reuses_a_discovered_dir_and_defaults_to_the_skills_dir() {
        let cfg = MemConfig::default();
        let a = adapter(Path::new("/h"), &cfg);
        let disc = DiscoveredPlacement {
            path: PathBuf::from("/h/skills/pr-describe"),
            layer: Some(LAYER_USER.to_owned()),
        };
        assert_eq!(
            a.placement_for("topos_abc", PlacementNaming::default(), Some(&disc))
                .dir,
            PathBuf::from("/h/skills/pr-describe")
        );
        assert_eq!(
            a.placement_for("topos_abc", PlacementNaming::default(), None)
                .dir,
            PathBuf::from("/h/skills/topos_abc")
        );
    }
}
