//! The `ClaudeCode` [`HarnessAdapter`] — discovery and byte-exact placement targeting — plus [`SPEC`],
//! the parameterization its idempotent session-start **auto-update trigger** in
//! `~/.claude/settings.json` runs under.
//!
//! Content-blind: it reads skill *directories* only to confirm a `SKILL.md` exists (never the bytes,
//! never the frontmatter), and it writes nothing at all. The trigger is a [`JsonHooksSpec`] instance
//! over the shared strict-JSON merge ([`crate::triggers`]) — built by
//! [`crate::triggers::adapter_for_slug`], like every other harness's — so the sentinel-keyed ownership,
//! adopt-or-leave, in-place migration of an older managed shape, and prune-only-what-we-emptied removal
//! are the SAME machinery every JSON-config harness runs. This harness declares only what is its own:
//! the `async` handler field and the `--hook claude-code` dialect marker.

use std::path::PathBuf;

use topos_types::{CurrencyKind, HarnessId, TriggerState};

use crate::triggers::cc_hooks::JsonHooksSpec;
use crate::{DiscoveredPlacement, HarnessAdapter, PlacementNaming, PlacementTarget};

/// The user-scope layer label recorded for a discovered/placed Claude Code skill. (`project`,
/// `enterprise`, … become representable later without a contract change — `DiscoveredPlacement.layer`
/// is already `Option<String>`.)
pub(crate) const LAYER_USER: &str = "user";

/// The per-hook timeout (seconds). A real sweep makes network calls (one delivery call per enrolled
/// workspace, plus fetches when a pointer moved), so this must cover a slow-but-working plane — while a
/// dead or stalling one must never hold the session start hostage: the client bounds its own connect/
/// response/body timeouts and trips a plane-down circuit breaker on the first connect failure, so one
/// minute is generous headroom, not the expected cost. The hook also runs `async`, so even the worst
/// case never blocks the session.
const HOOK_TIMEOUT_SECS: u64 = 60;

/// Claude Code's auto-update trigger: a matcher-free `SessionStart` entry in `settings.json` carrying
/// the one guarded sweep. An omitted matcher fires on EVERY SessionStart source — startup, resume,
/// clear, compact — and the quiet sweep's own TTL makes the redundant fires cheap.
///
/// Two fields are this harness's alone. `handler_async` runs the hook off the session's critical path,
/// so even a stalled sweep never blocks a session event. **`hook_dialect` is what opts THIS harness
/// into the `reloadSkills` extension** — the field that makes Claude Code re-scan its skill dirs
/// same-session, so pulled bytes are live immediately (any person-facing line rides the same
/// document's context injection). It is a marker, not a behavior switch: a trigger that does NOT carry
/// it gets a schema-conservative document (`hookEventName` + `additionalContext` only, and nothing at
/// all when there is nothing to read), because other harnesses validate hook stdout against a strict
/// schema and reject unknown fields — Codex fails the whole session-start hook on one. Conservative is
/// therefore the default every trigger gets, and each harness proven to understand an extension names
/// itself here.
///
/// The written state is the whole evidence — Claude Code gates no hook behind a separate consent — so
/// a successful registration honestly reports `Active`. Ownership keys on the sentinel ALONE, so
/// changing the command's spelling stays free: a re-arm migrates any earlier spelling in place.
/// Marker schema 2 = the async, every-SessionStart-source entry.
pub(crate) static SPEC: JsonHooksSpec = JsonHooksSpec {
    slug: "claude-code",
    marker_id: "topos:claude-code:currency:2",
    config_file: "settings.json",
    events_path: &["hooks"],
    event: "SessionStart",
    grouped: true,
    matcher: None,
    handler_type: true,
    command_key: "command",
    timeout: Some(("timeout", HOOK_TIMEOUT_SECS)),
    handler_async: true,
    hook_dialect: Some("claude-code"),
    root_seed: None,
    command_sentinel: true,
    live_kind: CurrencyKind::SessionStart,
    placed_state: TriggerState::Active,
    note: None,
};

/// The [`HarnessAdapter`] for Claude Code. Holds only the resolved config home (injected, so tests
/// point it at a temp dir): placement reads directories and writes nothing, so this side needs no
/// [`crate::ConfigStore`] — the trigger half built from this module's spec holds that port.
pub struct ClaudeCode {
    /// `$CLAUDE_CONFIG_DIR` (Claude Code's own override) else `$HOME/.claude`.
    home: PathBuf,
}

impl std::fmt::Debug for ClaudeCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCode")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl ClaudeCode {
    /// Construct over an explicit config home. Production passes [`ClaudeCode::resolve_home`]; tests
    /// pass a temp dir so the real `~/.claude` is never read.
    #[must_use]
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    /// Resolve Claude Code's config home exactly as Claude Code does — `$CLAUDE_CONFIG_DIR` if set,
    /// else `$HOME/.claude` — through the registry's ONE resolver, so this and the detection probe
    /// can never disagree about where the harness lives. Editing any other path would touch a
    /// `settings.json` Claude Code never reads.
    #[must_use]
    pub fn resolve_home() -> PathBuf {
        crate::registry::config_root(
            crate::registry::Root::ClaudeHome,
            &crate::registry::real_home(),
        )
    }

    fn skills_dir(&self) -> PathBuf {
        self.home.join("skills")
    }

    /// The no-discovery placement dir for a pure follower's first receive. Claude Code invokes a skill by
    /// its FOLDER name, so prefer the skill's real (sanitized) display name; on a collision with a
    /// DIFFERENT existing dir (another skill, or the user's own), disambiguate by the workspace slug; fall
    /// back to the globally-unique `skill_id` (which can never collide). Only a FREE dir is ever chosen,
    /// so a first receive never clobbers an existing skill. The `naming` strings are untrusted and are
    /// sanitized to a single safe component before any join. The rules live in the crate-level
    /// [`crate::choose_skill_dir`] (this adapter is the reference; other placement targets name
    /// identically); with no placement record to consult here, only a FREE dir counts as available.
    fn follower_placement_dir(&self, skill_id: &str, naming: PlacementNaming<'_>) -> PathBuf {
        crate::choose_skill_dir(
            &self.skills_dir(),
            skill_id,
            naming,
            &crate::dir_taken,
            &|_| false,
        )
    }
}

impl HarnessAdapter for ClaudeCode {
    fn id(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }

    /// The ONE skill-directory probe ([`crate::registry::discover_skill_dirs`]) over this harness's
    /// skills dir — sorted, dot-entries skipped, a root `SKILL.md` the only confirmation.
    fn discover(&self) -> Vec<DiscoveredPlacement> {
        crate::registry::discover_skill_dirs(&self.skills_dir())
            .into_iter()
            .map(|path| DiscoveredPlacement {
                path,
                layer: Some(LAYER_USER.to_owned()),
            })
            .collect()
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConfigStore;
    use crate::triggers::SENTINEL;
    use crate::triggers::TriggerAdapter as _;
    use crate::triggers::cc_hooks::JsonHooks;
    use serde_json::Value;
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

    fn adapter(home: &Path) -> ClaudeCode {
        ClaudeCode::new(home.to_path_buf())
    }

    /// This harness's TRIGGER half — the shared JSON-hooks machinery under [`SPEC`], built exactly as
    /// [`crate::triggers::adapter_for_slug`] builds it for the real machine.
    fn trigger<'a>(home: &Path, cfg: &'a MemConfig) -> JsonHooks<'a> {
        JsonHooks::new(&SPEC, home.to_path_buf(), cfg)
    }

    /// The exact command [`SPEC`] registers — the shared shell sweep plus this harness's dialect
    /// marker, composed by the one base rather than restated here.
    fn hook_command() -> String {
        crate::triggers::cc_hooks::sweep_command(&SPEC)
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
            \"command\": \"command -v topos >/dev/null 2>&1 && topos install --quiet --hook claude-code || true  # topos:currency\",
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
    fn the_hook_command_is_the_shared_shell_sweep_plus_the_dialect_marker() {
        // The two spellings must never drift apart silently: this harness's command IS the breadth
        // surfaces' shell sweep line with ONE addition — the `--hook claude-code` marker that opts
        // it into the `reloadSkills` extension. Every other harness stays on the unmarked line,
        // whose answer is the schema-conservative document a strict validator accepts.
        assert_eq!(
            hook_command(),
            crate::triggers::SHELL_SWEEP_LINE.replace(
                "topos install --quiet",
                "topos install --quiet --hook claude-code"
            ),
            "the Claude Code command is the shared sweep line + the dialect marker"
        );
        assert!(hook_command().ends_with(SENTINEL), "ownership keys on this");
    }

    #[test]
    fn install_into_absent_settings_writes_the_exact_managed_hook() {
        let cfg = MemConfig::default(); // absent
        let home = PathBuf::from("/nonexistent-claude-home");
        let report = trigger(&home, &cfg).install();

        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.agent, "claude-code");
        assert_eq!(report.currency_kind, CurrencyKind::SessionStart);
        assert_eq!(report.marker_id, SPEC.marker_id);
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
        trigger(&home, &cfg).install();
        let after_first = cfg.text();

        let report = trigger(&home, &cfg).install();
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

        let report = trigger(&PathBuf::from("/h"), &cfg).install();
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
            cmd,
            hook_command(),
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
        let again = trigger(&PathBuf::from("/h"), &cfg).install();
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
            hook_command().replace('"', "\\\"")
        ));
        let report = trigger(&PathBuf::from("/h"), &cfg).install();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(cfg.writes(), 1, "one migrating write");

        let root: Value = serde_json::from_str(&cfg.text().unwrap()).unwrap();
        let groups = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "rewritten in place, never duplicated");
        assert!(groups[0].get("matcher").is_none(), "matcher shed");
        assert_eq!(groups[0]["hooks"][0]["async"], true);

        let again = trigger(&PathBuf::from("/h"), &cfg).install();
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
        let report = trigger(&PathBuf::from("/h"), &cfg).install();
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
        assert_eq!(ours[0]["command"].as_str().unwrap(), hook_command());
        assert_eq!(ours[0]["async"], true);

        // Idempotent: the relocated shape is canonical — a re-run writes nothing.
        let again = trigger(&PathBuf::from("/h"), &cfg).install();
        assert!(again.touched_path.is_none(), "no second write");
        assert_eq!(cfg.writes(), 1);
        assert_eq!(
            cfg.text().unwrap().matches(SENTINEL).count(),
            1,
            "exactly one managed handler — never duplicated by the relocation"
        );
    }

    #[test]
    fn rearming_over_an_old_pull_hook_replaces_it_with_exactly_one_current_hook() {
        // A config already holding the FULL old managed hook line — the `topos pull --quiet` command
        // carrying our sentinel. Re-arming must recognize it (sentinel alone), rewrite it to the
        // current `topos install` command IN PLACE, and never append a second managed group beside
        // it.
        let old =
            "command -v topos >/dev/null 2>&1 && topos pull --quiet || true  # topos:currency";
        let cfg = MemConfig::with(&format!(
            "{{\"hooks\":{{\"SessionStart\":[{{\"matcher\":\"startup\",\"hooks\":[{{\"type\":\"command\",\"command\":\"{old}\",\"timeout\":60}}]}}]}}}}"
        ));

        let report = trigger(&PathBuf::from("/h"), &cfg).install();
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
            text.contains("topos install --quiet --hook claude-code"),
            "rewritten to the current install command, carrying the dialect marker"
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
            hook_command(),
            "the single managed hook is the canonical update command"
        );
    }

    #[test]
    fn install_preserves_foreign_keys_and_other_hooks() {
        let cfg = MemConfig::with(
            "{\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"PreToolUse\": [{\"matcher\": \"Bash\"}]\n  }\n}\n",
        );
        let report = trigger(&PathBuf::from("/h"), &cfg).install();
        assert_eq!(report.state, TriggerState::Active);

        let root: Value = serde_json::from_str(&cfg.text().unwrap()).unwrap();
        assert_eq!(root["model"], "opus", "foreign top-level key survives");
        assert!(
            root["hooks"]["PreToolUse"].is_array(),
            "sibling hook survives"
        );
        assert!(
            root["hooks"]["SessionStart"][0]["hooks"][0]["command"]
                .as_str()
                .is_some_and(|c| c.contains(SENTINEL)),
            "our hook was added"
        );
    }

    /// Adopt-or-leave recognizes the hook a person would write TODAY, not only the verb an older
    /// build taught: either spelling, sentinel-free, is somebody else's hook — left alone, never
    /// joined by a managed second copy that would sweep twice per session.
    #[test]
    fn install_leaves_a_hand_rolled_sweep_unmanaged_in_either_verb() {
        for hand_rolled in ["topos update --quiet", "topos pull"] {
            let cfg = MemConfig::with(&format!(
                "{{\"hooks\":{{\"SessionStart\":[{{\"hooks\":[{{\"type\":\"command\",\"command\":\"{hand_rolled}\"}}]}}]}}}}"
            ));
            let report = trigger(&PathBuf::from("/h"), &cfg).install();
            assert_eq!(
                report.state,
                TriggerState::AlreadyPresentUnmanaged,
                "{hand_rolled}"
            );
            assert!(report.touched_path.is_none(), "{hand_rolled}");
            assert_eq!(
                cfg.writes(),
                0,
                "{hand_rolled}: never blind-append next to a user's own hook"
            );
        }
    }

    #[test]
    fn install_fails_closed_on_malformed_or_wrong_typed_config() {
        // Malformed JSON → degrade, no write.
        let bad = MemConfig::with("{ this is not json ");
        let r = trigger(&PathBuf::from("/h"), &bad).install();
        assert_eq!(r.state, TriggerState::Degraded);
        assert_eq!(bad.writes(), 0);
        assert_eq!(
            bad.text().as_deref(),
            Some("{ this is not json "),
            "untouched"
        );

        // `hooks` present but the wrong type → degrade, no write.
        let wrong = MemConfig::with("{\"hooks\": \"oops\"}");
        let r = trigger(&PathBuf::from("/h"), &wrong).install();
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
        let report = trigger(&PathBuf::from("/h"), &cfg).install();
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
            hook_command(),
            "normalized to the canonical managed command"
        );
    }

    #[test]
    fn remove_scrubs_our_entry_and_keeps_siblings_then_is_idempotent() {
        let cfg = MemConfig::with(
            "{\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"PreToolUse\": [{\"matcher\": \"Bash\"}]\n  }\n}\n",
        );
        let home = PathBuf::from("/h");
        trigger(&home, &cfg).install();
        assert!(cfg.text().unwrap().contains("topos install"));

        let report = trigger(&home, &cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        let root: Value = serde_json::from_str(&cfg.text().unwrap()).unwrap();
        assert_eq!(root["model"], "opus", "foreign key survives the scrub");
        assert!(
            root["hooks"]["PreToolUse"].is_array(),
            "sibling hook survives"
        );
        assert!(
            root["hooks"].get("SessionStart").is_none(),
            "our SessionStart group was pruned away (we created it)"
        );

        // Idempotent: a second remove is a clean no-op.
        let writes_before = cfg.writes();
        let report = trigger(&home, &cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(cfg.writes(), writes_before, "second remove writes nothing");
    }

    #[test]
    fn remove_leaves_a_hand_rolled_topos_pull_and_an_absent_file_alone() {
        // A user's own `topos pull` (no marker) is never blind-removed.
        let cfg = MemConfig::with(
            "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"topos pull\"}]}]}}",
        );
        let report = trigger(&PathBuf::from("/h"), &cfg).remove();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0);

        // An absent settings file → a clean no-op, never created.
        let absent = MemConfig::default();
        let report = trigger(&PathBuf::from("/h"), &absent).remove();
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
        trigger(&home, &cfg).install(); // appends our dedicated group
        let report = trigger(&home, &cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);

        let root: Value = serde_json::from_str(&cfg.text().unwrap()).unwrap();
        let groups = root["hooks"]["SessionStart"]
            .as_array()
            .expect("the user's SessionStart group survives");
        assert_eq!(groups.len(), 1, "only OUR group was pruned");
        assert_eq!(
            groups[0]["matcher"], "resume",
            "the user's empty group is left intact"
        );
    }

    #[test]
    fn remove_degrades_on_malformed_without_clobbering() {
        let cfg = MemConfig::with("{ not json ");
        let report = trigger(&PathBuf::from("/h"), &cfg).remove();
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

        let found = adapter(&home.0).discover();
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
        let found = adapter(&PathBuf::from("/no-such-claude-home-xyz")).discover();
        assert!(found.is_empty());
    }

    #[test]
    fn placement_for_reuses_a_discovered_dir() {
        let a = adapter(&PathBuf::from("/h"));
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
        // A home with nothing on disk, so no candidate ever collides.
        let a = adapter(&PathBuf::from("/nonexistent-home"));
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
        let a = adapter(&PathBuf::from("/h"));
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
        let a = adapter(&home.0);

        // Collision on the plain name → suffixed by the (sanitized) workspace slug — skill first, so
        // the folder stays recognizable — never clobbering.
        let naming = PlacementNaming {
            name: Some("deploy-helper"),
            workspace_slug: Some("robert's workspace"),
        };
        assert_eq!(
            a.placement_for("topos_abc", naming, None).dir,
            home.0
                .join("skills")
                .join("deploy-helper-robert-s-workspace"),
        );

        // Now the namespaced form is taken too → the COUNTED named form; the opaque id never
        // materializes as a directory name a human has to read.
        home.skill("deploy-helper-robert-s-workspace");
        assert_eq!(
            a.placement_for("topos_abc", naming, None).dir,
            home.0
                .join("skills")
                .join("deploy-helper-robert-s-workspace-2"),
            "every collision stays name-led",
        );
    }

    #[test]
    fn deep_collisions_count_up_and_never_surface_the_opaque_id() {
        let home = TempHome::new();
        home.skill("deploy-helper"); // the plain name is taken…
        home.skill("deploy-helper-acme"); // …and its workspace form…
        home.skill("deploy-helper-acme-2"); // …and the first counted form.
        let a = adapter(&home.0);
        let naming = PlacementNaming {
            name: Some("deploy-helper"),
            workspace_slug: Some("acme"),
        };
        let dir = a.placement_for("topos_abc", naming, None).dir;
        assert_eq!(
            dir,
            home.0.join("skills").join("deploy-helper-acme-3"),
            "the counted ladder stays name-led however deep the collision"
        );
        assert!(
            !dir.to_string_lossy().contains("topos_abc"),
            "the opaque id never becomes a directory name"
        );
    }

    #[test]
    fn a_dangling_symlink_counts_as_occupied_and_the_ladder_moves_on() {
        // `Path::exists()` traverses symlinks, so a DANGLING link would read as "free" and the
        // materializer would then refuse the unresolvable entry — occupancy is lstat-based, and
        // the ladder namespaces past it instead.
        let home = TempHome::new();
        let skills = home.0.join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::os::unix::fs::symlink(skills.join("gone-target"), skills.join("deploy-helper"))
            .unwrap();
        let a = adapter(&home.0);
        let naming = PlacementNaming {
            name: Some("deploy-helper"),
            workspace_slug: Some("acme"),
        };
        assert_eq!(
            a.placement_for("topos_abc", naming, None).dir,
            home.0.join("skills").join("deploy-helper-acme"),
            "a dangling link at the by-name rung is occupied — the ladder suffixes past it"
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
    fn the_artifact_is_disclosed_only_when_our_entry_is_present() {
        use crate::triggers::TriggerArtifact;
        let cfg = MemConfig::default();
        let home = PathBuf::from("/h");
        assert!(
            trigger(&home, &cfg).artifacts().is_empty(),
            "no entry → nothing disclosed"
        );
        trigger(&home, &cfg).install();
        assert_eq!(
            trigger(&home, &cfg).artifacts(),
            vec![TriggerArtifact::Path(PathBuf::from("/h/settings.json"))],
            "our entry present → settings.json disclosed (never deleted)"
        );
    }
}
