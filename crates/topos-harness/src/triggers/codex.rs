//! `codex` — the session-start auto-update hook in `<root>/hooks.json` (production root:
//! `$CODEX_HOME` else `~/.codex`; in a project, `<project>/.codex`).
//!
//! `hooks.json` is an ordinary member of the strict-JSON family ([`super::cc_hooks`]):
//! Claude-Code-shaped matcher-free groups under a top-level `"hooks"` map, event key
//! `"SessionStart"`, handler `{"type": "command", "command": <the guarded sweep>,
//! "timeout": 60}` — so the sentinel-keyed ownership, adopt-or-leave, in-place migration and
//! prune-only-what-we-emptied removal are the same machinery every other JSON harness runs.
//! Nothing else is written: codex reads hooks by default in current builds, so its
//! `config.toml` is never opened, let alone edited.
//!
//! Codex's hooks used to live in a `[[hooks.SessionStart]]` block topos appended to `config.toml`
//! itself, and a later build set `[features] hooks = true` there. Both mechanisms are gone. There
//! is no migration and no cleanup: a block or a switch an earlier build wrote is dead bytes in a
//! file this adapter no longer touches.
//!
//! **Evidence level:** the `hooks.json` shape is taken from herdr's shipped Codex integration
//! (Apache-2.0), which writes this schema; no live build was probed here.
//!
//! **Consent posture — `Active` is NEVER claimed:** codex gates hooks behind persisted
//! per-definition trust granted in its own UI, and that trust store is not readable evidence. A
//! successful install reports `Inactive` + the explicit-pull floor with a note naming the step
//! still owed; the kind when the hook would fire is `SessionStart`. This adapter never writes
//! codex's trust state. In a project the step is the repo trust codex asks for on first use, and
//! the note says so.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, TriggerState};

use crate::ConfigStore;

use crate::registry::{self, Root};

use super::cc_hooks::{JsonHooks, JsonHooksSpec};

/// The consent step still owed after a successful user-level registration (codex's own trust
/// prompt).
const NOTE: &str =
    "trust the hook inside Codex (it will prompt) — until then, explicit `topos update`";

/// The consent step still owed in a PROJECT: codex runs a repo's hooks only once the repo is
/// trusted in codex.
const PROJECT_NOTE: &str = "Codex runs it once you trust this repo";

/// The hook entry itself. Schema 2 = the `hooks.json` entry (schema 1 was the retired
/// `config.toml` block). `placed_state` is `Inactive` by construction: codex's trust prompt is
/// owed no matter how the write went.
pub(crate) static SPEC: JsonHooksSpec = USER_SPEC;

/// The same entry in ONE PROJECT's `.codex/hooks.json`: the same marker and command, the note
/// naming the repo-trust step instead of the per-hook prompt.
pub(crate) static PROJECT_SPEC: JsonHooksSpec = JsonHooksSpec {
    note: Some(PROJECT_NOTE),
    ..USER_SPEC
};

const USER_SPEC: JsonHooksSpec = JsonHooksSpec {
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
    command_sentinel: true,
    live_kind: CurrencyKind::SessionStart, // what fires when live; never reported live (see above)
    placed_state: TriggerState::Inactive,
    note: Some(NOTE),
};

/// Production root: `$CODEX_HOME` (codex's own override, resolved the way the registry does)
/// else `~/.codex` under the passed home.
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    registry::config_root(Root::CodexHome, home)
}

pub(crate) fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> JsonHooks<'a> {
    JsonHooks::new(&SPEC, resolve_root(home), cfg)
}

/// The same trigger in ONE PROJECT: `<project>/.codex/hooks.json`.
pub(crate) fn in_project<'a>(root: &Path, cfg: &'a dyn ConfigStore) -> JsonHooks<'a> {
    JsonHooks::new(&PROJECT_SPEC, root.join(".codex"), cfg)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{ErrConfig, MemConfig};
    use super::super::{SENTINEL, TriggerAdapter, TriggerArtifact};
    use super::*;

    fn a<'c>(cfg: &'c MemConfig) -> JsonHooks<'c> {
        JsonHooks::new(&SPEC, PathBuf::from("/x"), cfg)
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
            \"command\": \"command -v topos >/dev/null 2>&1 && topos install --quiet --from codex || true  # topos:currency\",
            \"timeout\": 60,
            \"type\": \"command\"
          }
        ]
      }
    ]
  }
}
";

    /// The entry lands, `Active` is never claimed, and `config.toml` is never created: codex reads
    /// hooks without a switch, so the one file topos writes is the entry's own.
    #[test]
    fn fresh_install_writes_the_entry_only_and_never_claims_active() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.agent, "codex");
        assert_eq!(report.marker_id, "topos:codex:currency:2");
        // Codex gates hooks behind its own trust prompt — Active is never claimed.
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
        assert!(report.note.as_deref().unwrap().contains("trust the hook"));
        assert_eq!(report.touched_path.as_deref(), Some(HOOKS));
        assert_eq!(cfg.text(HOOKS).as_deref(), Some(HOOKS_FIXTURE));
        assert!(cfg.text(CONFIG).is_none(), "config.toml is never created");
        assert_eq!(cfg.writes(), 1, "one file, no more");
    }

    /// A `config.toml` a person already has is not opened: whatever it says about `features`,
    /// the install reads and writes `hooks.json` alone.
    #[test]
    fn an_existing_config_toml_is_never_touched() {
        let before = "model = \"gpt-5-codex\"\n\n[features]\nhooks = false\n";
        let cfg = MemConfig::with_file(CONFIG, before);
        assert_eq!(a(&cfg).install().state, TriggerState::Inactive);
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(before), "byte-untouched");
        assert_eq!(cfg.writes(), 1, "the entry only");
        a(&cfg).remove();
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(before), "byte-untouched");
    }

    #[test]
    fn install_is_idempotent_a_true_no_op_on_rerun() {
        let cfg = MemConfig::default();
        a(&cfg).install();
        let after_first = cfg.text(HOOKS);
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(
            report.note.is_some(),
            "the consent note rides the no-op too"
        );
        assert!(report.touched_path.is_none());
        assert_eq!(cfg.writes(), 1, "second install writes nothing");
        assert_eq!(cfg.text(HOOKS), after_first);
    }

    /// A malformed `hooks.json` degrades with zero writes.
    #[test]
    fn a_malformed_hooks_file_degrades_with_zero_writes() {
        let cfg = MemConfig::with_file(HOOKS, "{ nope");
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Degraded);
        assert_eq!(cfg.writes(), 0);
    }

    /// Somebody's own sweep hook is adopted-or-left.
    #[test]
    fn a_hand_rolled_topos_hook_is_adopt_or_leave() {
        let cfg = MemConfig::with_file(
            HOOKS,
            "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"topos update --quiet\"}]}]}}",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0);
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
        assert!(text.contains(&super::super::cc_hooks::sweep_command(&SPEC)));
    }

    #[test]
    fn remove_scrubs_the_entry_then_is_idempotent() {
        let cfg = MemConfig::default();
        a(&cfg).install();

        let report = a(&cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
        let root: serde_json::Value = serde_json::from_str(&cfg.text(HOOKS).unwrap()).unwrap();
        assert!(
            root.get("hooks").is_none(),
            "the map we created is pruned back"
        );

        let writes = cfg.writes();
        assert_eq!(a(&cfg).remove().state, TriggerState::Inactive);
        assert_eq!(cfg.writes(), writes, "second remove writes nothing");

        // A remove over an absent install is a clean no-op that creates nothing.
        let absent = MemConfig::default();
        assert_eq!(a(&absent).remove().state, TriggerState::Inactive);
        assert!(absent.text(HOOKS).is_none());
    }

    /// Disclosure: the entry's file, only while it holds our entry.
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

    /// The project spec is the user spec at a project root, with the repo-trust note.
    #[test]
    fn the_project_entry_is_the_same_bytes_under_the_repo_trust_note() {
        let cfg = MemConfig::default();
        let report = in_project(Path::new("/p"), &cfg).install();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.note.as_deref(), Some(PROJECT_NOTE));
        assert_eq!(report.touched_path.as_deref(), Some("/p/.codex/hooks.json"));
        assert_eq!(
            cfg.text("/p/.codex/hooks.json").as_deref(),
            Some(HOOKS_FIXTURE)
        );
        assert!(cfg.text("/p/.codex/config.toml").is_none());
    }

    #[test]
    fn an_unreadable_store_degrades_with_zero_writes() {
        let cfg = ErrConfig;
        let adapter = JsonHooks::new(&SPEC, PathBuf::from("/x"), &cfg);
        assert_eq!(adapter.install().state, TriggerState::Degraded);
        assert_eq!(adapter.remove().state, TriggerState::Degraded);
        assert!(!adapter.present());
    }
}
