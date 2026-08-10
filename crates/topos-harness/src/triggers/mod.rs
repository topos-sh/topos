//! `triggers` — auto-update triggers: ONE [`TriggerAdapter`] port covering every trigger-capable
//! harness, all running the ONE sweep (`topos update --quiet`, which self-throttles client-side, so
//! session-shaped re-fires are cheap). The sweep is SCHEMA-CONSERVATIVE unless a harness declares a
//! hook dialect: an unmarked command answers with `hookEventName` + `additionalContext` only,
//! because most harnesses are not proven to tolerate more and one of them (codex) rejects unknown
//! hook-output fields outright.
//!
//! [`adapter_for_slug`] is the ONE place a harness's trigger machinery is named, so the set of
//! trigger-capable harnesses is a VIEW over the [`registry`](crate::registry) table — a caller arms
//! or scrubs by iterating rows, never by carrying its own list. Three shared bases carry the
//! machinery, plus the harnesses whose trigger rides their full
//! [`HarnessAdapter`]:
//!
//! - `cc_hooks` — the JSON-config-merge family (Claude-Code-shaped hooks registered in a shared
//!   strict-JSON config file): `claude-code` (whose spec lives with its adapter, and is the only one
//!   declaring a hook dialect), `gemini-cli`, `cursor`, `droid`.
//! - `file_drop` — one topos-owned file at a harness-defined path: `github-copilot`, `opencode`,
//!   `goose`, `amp`, `cline`.
//! - `codex` is special: its config is TOML, handled as a line-anchored merge mirroring the
//!   Hermes YAML discipline — provable shapes only, fail-closed on everything else.
//! - `openclaw` (its own scheduler) and `hermes-agent` (its own config edit) answer through their
//!   adapters, wrapped so a caller never sees the difference.
//!
//! Every adapter here mirrors the big-three idiom: content-blind, an injected home (never the
//! real `~`), durable writes through the [`ConfigStore`] port, sentinel/marker-keyed ownership,
//! adopt-or-leave on foreign artifacts, and fail-closed (`Degraded`, ZERO writes) on any shape it
//! cannot prove. Honesty is structural: `Active` is claimed only on the per-instance evidence
//! documented in each instance module; a registration whose harness gates hooks behind its own
//! consent (and whose consent store is not readable evidence) reports `Inactive` with the
//! [`topos_types::CurrencyKind::ExplicitPullOnly`] floor and a note naming the consent step still owed. No
//! adapter ever WRITES another program's trust/consent state — at most it reads it, fail-closed,
//! as evidence.

use std::path::Path;

use topos_types::TriggerReport;

use crate::{CommandRunner, ConfigStore, HarnessAdapter};

mod amp;
pub(crate) mod cc_hooks;
mod cline;
mod codex;
mod cursor;
mod droid;
mod file_drop;
mod gemini_cli;
mod github_copilot;
mod goose;
mod opencode;
#[cfg(test)]
pub(crate) mod testutil;

/// The version-agnostic ownership sentinel — the exact spelling the Claude Code reference
/// adapter writes as a trailing shell comment, reused verbatim by every shell-string surface
/// here (and by the line-anchored TOML merge as its block anchor line), so ownership detection
/// stays ONE substring across every build and every surface.
pub(crate) const SENTINEL: &str = "# topos:currency";

/// The guarded sweep WITHOUT the sentinel suffix. The `command -v` guard skips the update when
/// the `topos` binary is gone (post-uninstall safety) and the `|| true` tail makes the whole
/// line exit 0 regardless, so a best-effort update sweep never surfaces as a hook error.
/// Quoted-string surfaces (the TOML block) register this form; their sentinel rides a separate
/// comment line instead of an in-command suffix.
pub(crate) const GUARDED_SWEEP: &str =
    "command -v topos >/dev/null 2>&1 && topos update --quiet || true";

/// The ONE shell-string sweep line every shell surface here registers: the guarded sweep + the
/// trailing ownership sentinel (inert under `sh -c`).
///
/// It carries NO `--hook <harness>` marker, and that is the point: unmarked is the
/// schema-conservative default. A changed sweep then answers with `hookEventName` +
/// `additionalContext` only — or, with nothing a person must read, nothing at all — the shape a
/// strict hook-output validator accepts (Codex rejects any unknown field and paints the whole
/// session-start hook as failed). Only a harness proven to understand an extension names itself
/// with `--hook`; the Claude Code reference adapter does, and its command is otherwise this exact
/// line — pinned against drift by that adapter's
/// `the_hook_command_is_the_shared_shell_sweep_plus_the_dialect_marker` test.
pub(crate) const SHELL_SWEEP_LINE: &str =
    "command -v topos >/dev/null 2>&1 && topos update --quiet || true  # topos:currency";

/// The plain argv sweep for non-shell / code surfaces: plugin code runs this and swallows
/// failures itself (no shell guard is possible there, and none is needed — the plugin's own
/// try/catch is the exit-0 tail's analog). Unmarked, so the conservative dialect applies.
pub(crate) const PLAIN_SWEEP: &str = "topos update --quiet";

/// Whether a command a person wrote themselves INVOKES a topos auto-update sweep — the
/// hand-rolled-hook probe every trigger engine runs on a command carrying NO [`SENTINEL`], so such
/// an entry is adopted-or-left rather than duplicated beside.
///
/// It recognizes BOTH verb spellings. The current one is what a person copies out of the docs
/// today; the legacy one is what earlier builds taught, and a machine still carrying it must not
/// grow a second, managed entry the moment it is re-armed. Ownership is never decided here — that
/// keys on the sentinel alone.
///
/// **A mention is not an invocation.** `notify-send "topos update failed last night"` names the
/// verb without running it, and reading that as somebody's own hook would leave the machine with no
/// topos trigger at all — silently, because adopt-or-leave writes nothing and reports success. So
/// the phrase must sit where a command can START (the string's beginning, a new line, or after an
/// operator that opens one) and END at a token boundary, which is what [`invokes_at_command_start`]
/// decides.
///
/// Two residuals, both deliberate, both failing toward the safer side (a duplicate entry a person
/// can see, never a silently absent trigger): a PATH-spelled invocation (`/usr/local/bin/topos
/// update`) reads as not-hand-rolled, because accepting `/` as an opener would take
/// `notify-send "logs/topos update failed"` with it; and a command wrapped in an interpreter
/// (`sh -c "topos update"`) does too, since knowing which arguments of which programs are
/// themselves commands is a shell parser's job, not this predicate's.
pub(crate) fn is_hand_rolled_sweep(cmd: &str) -> bool {
    ["topos update", "topos pull"]
        .iter()
        .any(|verb| invokes_at_command_start(cmd, verb))
}

/// Whether `verb` occurs in `cmd` at a command position and ends at a token boundary. Every
/// occurrence is tried: a line may hold the guard, the sweep, and a tail.
fn invokes_at_command_start(cmd: &str, verb: &str) -> bool {
    let mut from = 0;
    while let Some(rel) = cmd[from..].find(verb) {
        let start = from + rel;
        let end = start + verb.len();
        // Ending at a token boundary is what tells `topos update` from `topos updated`; a flag or
        // an argument arrives after whitespace, so whitespace covers `--quiet`.
        let ends_token = cmd[end..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || matches!(c, ';' | '&' | '|' | ')' | '"' | '\''));
        if ends_token && opens_a_command(&cmd[..start]) {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Whether a command can START right after `prefix` — nothing before it, a line break, or a shell
/// operator that ends the previous command. One opening quote is transparent, so a quoted command
/// in command position still counts while a quoted MENTION (whose prefix keeps its own program
/// name) does not.
fn opens_a_command(prefix: &str) -> bool {
    let head = prefix.trim_end();
    // A newline anywhere in the whitespace run before the phrase starts a fresh command line.
    if prefix[head.len()..].contains('\n') {
        return true;
    }
    let head = head.strip_suffix(['"', '\'']).unwrap_or(head).trim_end();
    head.is_empty() || head.ends_with([';', '&', '|', '(', '{'])
}

/// The auto-update-trigger port for ONE registry-slug harness: idempotent (un)install of the one
/// sweep trigger, plus a provable-presence health probe. Every harness with a trigger is reachable
/// through this port — the config-merge and file-drop families here, and the harnesses whose trigger
/// rides their full [`HarnessAdapter`] — so a caller arming a machine never
/// needs to know which machinery serves which harness.
pub trait TriggerAdapter {
    /// The registry slug this adapter serves.
    fn slug(&self) -> &'static str;
    /// Idempotently install the auto-update trigger — a rerun over an already-canonical artifact
    /// writes nothing; anything unprovable degrades with zero writes.
    fn install(&self) -> TriggerReport;
    /// Surgically remove OUR trigger artifact (sentinel/marker-confirmed only — a foreign
    /// artifact is never touched); idempotent.
    fn remove(&self) -> TriggerReport;
    /// Provable presence of OUR trigger artifact right now (the health probe). Anything
    /// unprovable answers `false` — presence is never claimed on faith.
    fn present(&self) -> bool;
    /// Why [`Self::present`] cannot be answered without running the harness, when it cannot — the
    /// reason a read-only status reports "unknown" instead of probing. `None` (the default) means the
    /// probe is an honest offline read: a trigger that lives in the filesystem is provable there.
    fn offline_probe_refusal(&self) -> Option<&'static str> {
        None
    }
    /// Whether removing this trigger must reach OUTSIDE the filesystem, into the harness's own
    /// program. A filesystem artifact is scrubbed unconditionally — it can outlive the harness that
    /// read it — while an out-of-process scrub is attempted only where the harness still looks
    /// installed, so a machine that never had it is never probed. `false` (the default) is the
    /// filesystem case.
    fn scrub_needs_live_harness(&self) -> bool {
        false
    }
}

/// A full [`HarnessAdapter`] seen through the trigger port. The harnesses topos ships a whole adapter
/// for carry their trigger inside it (a config edit; a scheduler registration), and this is how they
/// answer [`adapter_for_slug`] like any other harness — so a caller arming or scrubbing a machine
/// iterates ONE list and never branches on which machinery a slug uses.
struct AdapterTrigger<'a> {
    slug: &'static str,
    adapter: Box<dyn HarnessAdapter + 'a>,
    offline_refusal: Option<&'static str>,
    needs_live_harness: bool,
}

impl TriggerAdapter for AdapterTrigger<'_> {
    fn slug(&self) -> &'static str {
        self.slug
    }
    fn install(&self) -> TriggerReport {
        self.adapter.install_currency_trigger()
    }
    fn remove(&self) -> TriggerReport {
        self.adapter.remove_currency_trigger()
    }
    fn present(&self) -> bool {
        self.adapter.trigger_present()
    }
    fn offline_probe_refusal(&self) -> Option<&'static str> {
        self.offline_refusal
    }
    fn scrub_needs_live_harness(&self) -> bool {
        self.needs_live_harness
    }
}

/// Construct the trigger adapter for a registry slug, over an injected home + the [`ConfigStore`] and
/// [`CommandRunner`] ports. `home` is the USER home dir; each adapter resolves its own harness root
/// under it through the registry's one resolver, so a harness's env override (`$CODEX_HOME`,
/// `$HERMES_HOME`, `$XDG_CONFIG_HOME`) is honored exactly as detection honored it. `None` = no trigger
/// support for that slug (a placement-only harness), which is also the answer to an unknown slug.
///
/// This is the ONE place a harness's trigger machinery is named: the enumeration of trigger-capable
/// harnesses is "the [`registry`](crate::registry) rows this answers `Some` for", so there is no second
/// list to drift. Tests construct the adapters over fully-injected roots instead, so no suite depends
/// on the real environment.
#[must_use]
pub fn adapter_for_slug<'a>(
    slug: &str,
    home: &Path,
    cfg: &'a dyn ConfigStore,
    run: &'a dyn CommandRunner,
) -> Option<Box<dyn TriggerAdapter + 'a>> {
    Some(match slug {
        "amp" => Box::new(amp::adapter(home, cfg)),
        // The reference harness is an ordinary instance of the shared JSON-hooks base — its own
        // adapter runs this very spec, so arming it here and arming it there are one code path.
        "claude-code" => Box::new(cc_hooks::JsonHooks::new(
            &crate::claude_code::SPEC,
            crate::registry::config_root(crate::registry::Root::ClaudeHome, home),
            cfg,
        )),
        "cline" => Box::new(cline::adapter(home, cfg)),
        "codex" => Box::new(codex::adapter(home, cfg)),
        "cursor" => Box::new(cursor::adapter(home, cfg)),
        "droid" => Box::new(droid::adapter(home, cfg)),
        "gemini-cli" => Box::new(gemini_cli::adapter(home, cfg)),
        "github-copilot" => Box::new(github_copilot::adapter(home, cfg)),
        "goose" => Box::new(goose::adapter(home, cfg)),
        "opencode" => Box::new(opencode::adapter(home, cfg)),
        "openclaw" => Box::new(AdapterTrigger {
            slug: "openclaw",
            adapter: Box::new(crate::OpenClaw::new(home.join(".openclaw"), cfg, run)),
            // The trigger lives in OpenClaw's SCHEDULER, not the filesystem: proving it there means
            // running `openclaw cron list`, which a read-only status must not do.
            offline_refusal: Some("presence needs a live scheduler query"),
            needs_live_harness: true,
        }),
        "hermes-agent" => Box::new(AdapterTrigger {
            slug: "hermes-agent",
            adapter: Box::new(crate::Hermes::new(
                crate::registry::config_root(crate::registry::Root::HermesHome, home),
                crate::Hermes::resolve_accept_hooks(),
                cfg,
            )),
            offline_refusal: None,
            needs_live_harness: false,
        }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::testutil::MemConfig;
    use super::*;
    use crate::trigger_report;

    /// The hand-rolled probe decides whether topos installs its trigger AT ALL on a machine, and a
    /// false positive is the silent failure: adopt-or-leave writes nothing and reports success, so
    /// a person whose hook merely NAMES the verb would be left with no auto-update and no sign of
    /// it. Invocations must match, mentions must not.
    #[test]
    fn the_hand_rolled_probe_matches_invocations_and_not_mentions() {
        for invocation in [
            "topos update",
            "topos update --quiet",
            "topos pull",
            GUARDED_SWEEP,
            PLAIN_SWEEP,
            "command -v topos >/dev/null 2>&1 && topos pull --quiet || true",
            "echo starting; topos update --quiet",
            "(topos update --quiet)",
            "setup && { topos update; }",
            "echo one\ntopos update --quiet\necho two",
            // A quoted command sitting in command position is still an invocation.
            "cd /tmp && \"topos update\"",
        ] {
            assert!(
                is_hand_rolled_sweep(invocation),
                "must be recognized: {invocation:?}"
            );
        }
        for mention in [
            // The reported case: a notification ABOUT the sweep is not the sweep.
            "notify-send \"topos update failed last night\"",
            "echo 'remember to run topos update'",
            "# topos update runs from the other hook",
            "log --message=\"topos pull skipped\"",
            // A token boundary, not a prefix match.
            "topos updated",
            "topos updates --all",
            "mytopos update",
            // Named as an argument, not run.
            "echo topos update",
        ] {
            assert!(
                !is_hand_rolled_sweep(mention),
                "must NOT be recognized: {mention:?}"
            );
        }
    }

    #[test]
    fn the_sweep_consts_compose() {
        assert_eq!(
            SHELL_SWEEP_LINE,
            format!("{GUARDED_SWEEP}  {SENTINEL}"),
            "the shell line is the guarded sweep + two spaces + the sentinel"
        );
        assert!(GUARDED_SWEEP.contains(PLAIN_SWEEP));
        assert!(GUARDED_SWEEP.starts_with("command -v topos"));
        assert!(GUARDED_SWEEP.ends_with("|| true"));
        // The breadth surfaces stay UNMARKED — the conservative hook-output dialect is what a
        // harness gets unless it declares itself, and none of these has been proven to accept
        // more (codex actively rejects unknown hook-output fields).
        for line in [GUARDED_SWEEP, SHELL_SWEEP_LINE, PLAIN_SWEEP] {
            assert!(
                !line.contains("--hook"),
                "{line}: no dialect marker on a breadth sweep"
            );
        }
    }

    /// A `CommandRunner` whose binary is absent — no suite ever spawns a real harness CLI.
    struct NoCli;
    impl CommandRunner for NoCli {
        fn run(&self, _p: &str, _a: &[&str]) -> std::io::Result<crate::RunOutput> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "absent"))
        }
    }

    /// The trigger-capable set is a VIEW over the registry, not a list beside it: a caller arming a
    /// machine iterates registry rows and asks THIS for an adapter, so there is no second enumeration
    /// to drift. Both families answer here — the config-merge/file-drop instances and the harnesses
    /// whose trigger rides their full `HarnessAdapter`.
    #[test]
    fn the_trigger_capable_harnesses_are_a_view_over_the_registry() {
        let cfg = MemConfig::default();
        let home = std::path::PathBuf::from("/no-such-home");
        let capable: Vec<&str> = crate::registry::known_harnesses()
            .iter()
            .filter(|h| adapter_for_slug(h.slug, &home, &cfg, &NoCli).is_some())
            .map(|h| h.slug)
            .collect();
        assert_eq!(
            capable,
            [
                "amp",
                "claude-code",
                "openclaw",
                "cline",
                "codex",
                "cursor",
                "droid",
                "gemini-cli",
                "github-copilot",
                "goose",
                "hermes-agent",
                "opencode",
            ],
            "registry-table order"
        );
        for slug in &capable {
            let adapter = adapter_for_slug(slug, &home, &cfg, &NoCli)
                .unwrap_or_else(|| panic!("{slug} is trigger-capable"));
            assert_eq!(adapter.slug(), *slug);
            assert!(
                !adapter.present(),
                "{slug}: nothing on an empty store is ever claimed present"
            );
        }
        // Placement-only harnesses — and an unknown slug — get no trigger adapter.
        for slug in ["zed", "warp", "augment", "not-a-harness", ""] {
            assert!(
                adapter_for_slug(slug, &home, &cfg, &NoCli).is_none(),
                "{slug}"
            );
        }
    }

    /// The two knobs a caller needs to stay honest without knowing the machinery: a trigger living in
    /// the harness's own scheduler refuses an offline presence answer and is scrubbed only where the
    /// harness looks installed; every filesystem artifact answers and is scrubbed unconditionally.
    #[test]
    fn only_an_out_of_process_trigger_refuses_the_offline_probe() {
        let cfg = MemConfig::default();
        let home = std::path::PathBuf::from("/no-such-home");
        let openclaw = adapter_for_slug("openclaw", &home, &cfg, &NoCli).expect("openclaw");
        assert!(openclaw.offline_probe_refusal().is_some());
        assert!(openclaw.scrub_needs_live_harness());
        for slug in ["claude-code", "hermes-agent", "cursor", "goose"] {
            let a = adapter_for_slug(slug, &home, &cfg, &NoCli).expect(slug);
            assert!(a.offline_probe_refusal().is_none(), "{slug}");
            assert!(!a.scrub_needs_live_harness(), "{slug}");
        }
    }

    #[test]
    fn a_report_advertises_only_the_floor_when_not_active() {
        use topos_types::{CurrencyKind, TriggerState};
        for state in [
            TriggerState::Inactive,
            TriggerState::Degraded,
            TriggerState::AlreadyPresentUnmanaged,
        ] {
            let out = trigger_report("cline", CurrencyKind::SessionStart, state, None, "m", None);
            assert_eq!(
                out.currency_kind,
                CurrencyKind::ExplicitPullOnly,
                "{state:?}"
            );
        }
        let live = trigger_report(
            "cline",
            CurrencyKind::SessionStart,
            TriggerState::Active,
            None,
            "m",
            None,
        );
        assert_eq!(live.currency_kind, CurrencyKind::SessionStart);
    }
}
