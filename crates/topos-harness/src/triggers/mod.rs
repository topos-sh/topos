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
//! machinery, plus the two harnesses whose trigger lives in a program of their own:
//!
//! - `cc_hooks` — the JSON-config-merge family (Claude-Code-shaped hooks registered in a shared
//!   strict-JSON config file): `claude-code` (whose spec lives with its placement adapter, and is the
//!   only one declaring a hook dialect), `gemini-cli`, `cursor`, `droid`.
//! - `file_drop` — one topos-owned file at a harness-defined path: `github-copilot`, `opencode`,
//!   `goose`, `amp`, `cline`.
//! - `codex` is special: its config is TOML, handled as a line-anchored merge mirroring the
//!   Hermes YAML discipline — provable shapes only, fail-closed on everything else.
//! - `openclaw` (its own scheduler) and `hermes-agent` (its own config edit) implement this port
//!   DIRECTLY on the same types that carry their placement half, so a caller never sees the
//!   difference.
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

use std::path::{Path, PathBuf};

use topos_types::TriggerReport;

use crate::{CommandRunner, ConfigStore};

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

/// ONE artifact an auto-update trigger's scrub reaches — the unit a preview names, so a disclosure
/// can never promise less than the apply touches.
///
/// A preview discloses INTENT, not presence: a path row is only ever emitted for an artifact an
/// adapter can confirm is ours right now, while an out-of-process row is emitted UNCONDITIONALLY
/// (proving a scheduler registration means running the harness, which a preview must not do) and
/// says so in its own wording.
///
/// The ordering is the disclosure's: paths first (sorted among themselves, exactly as the path-only
/// footprint sorted), then the out-of-process rows.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum TriggerArtifact {
    /// A topos-owned file outside any skill dir — the config the trigger lives in, or the file it
    /// dropped. Confirmed ours at the moment it is listed.
    Path(PathBuf),
    /// A registration inside the harness's OWN program (a scheduler job): no filesystem artifact
    /// exists to name, so the row names the harness whose program holds it. `harness` is the
    /// harness's DISPLAY name — it is rendered to a person.
    OutOfProcess { harness: &'static str },
}

impl TriggerArtifact {
    /// The path this artifact IS, or `None` for one that lives outside the filesystem — the filter
    /// a path-typed surface (`list --footprint`) applies.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Path(p) => Some(p),
            Self::OutOfProcess { .. } => None,
        }
    }

    /// Whether this artifact lives in the harness's own program rather than the filesystem.
    #[must_use]
    pub fn is_out_of_process(&self) -> bool {
        matches!(self, Self::OutOfProcess { .. })
    }
}

impl std::fmt::Display for TriggerArtifact {
    /// The ONE rendering both disclosure surfaces print: a path verbatim, an out-of-process
    /// registration as the sentence that names the harness, admits the artifact is unprobed, and
    /// says where the removal actually happens.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(p) => write!(f, "{}", p.to_string_lossy()),
            Self::OutOfProcess { harness } => write!(
                f,
                "the {harness} scheduled update job, if armed (removed through {harness}'s scheduler)"
            ),
        }
    }
}

/// The auto-update-trigger port for ONE registry-slug harness: idempotent (un)install of the one
/// sweep trigger, a provable-presence health probe, and the artifacts it discloses. Every
/// harness with a trigger is reachable through this port — the config-merge and file-drop families
/// here, and the two harnesses whose trigger lives in their own program — so a caller arming a
/// machine never needs to know which machinery serves which harness. Where a bundle's BYTES land is
/// the other port's ([`crate::HarnessAdapter`]); nothing here ever writes a skill dir.
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
    /// Every artifact a scrub of this trigger REACHES — what a preview names. A filesystem
    /// artifact is disclosed as a [`TriggerArtifact::Path`] only where it can be confirmed ours
    /// right now (never a skill file, and never a path `uninstall` DELETES: a shared config the
    /// trigger lives in is scrubbed surgically by [`Self::remove`], the file itself kept); a
    /// trigger living in the harness's OWN program has no path at all and names itself as a
    /// [`TriggerArtifact::OutOfProcess`] row instead — unprobed, because a preview discloses
    /// INTENT, not presence.
    fn artifacts(&self) -> Vec<TriggerArtifact>;
    /// Why [`Self::present`] cannot be answered without running the harness, when it cannot — the
    /// reason a read-only status reports "unknown" instead of probing. `None` (the default) means the
    /// probe is an honest offline read: a trigger that lives in the filesystem is provable there.
    fn offline_probe_refusal(&self) -> Option<&'static str> {
        None
    }
    /// The config file this trigger lives in — the path a person must fix by hand when a scrub
    /// DEGRADES. [`Self::artifacts`] cannot answer it: that surface discloses only what is
    /// provably topos's RIGHT NOW, and a config topos could not read or write is exactly the one
    /// it cannot prove anything about. `None` (the default) is the trigger that lives in the
    /// harness's own program instead of a file.
    fn config_file(&self) -> Option<PathBuf> {
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
        "claude-code" => claude_code_at(
            crate::registry::config_root(crate::registry::Root::ClaudeHome, home),
            cfg,
        ),
        "cline" => Box::new(cline::adapter(home, cfg)),
        "codex" => Box::new(codex::adapter(home, cfg)),
        "cursor" => Box::new(cursor::adapter(home, cfg)),
        "droid" => Box::new(droid::adapter(home, cfg)),
        "gemini-cli" => Box::new(gemini_cli::adapter(home, cfg)),
        "github-copilot" => Box::new(github_copilot::adapter(home, cfg)),
        "goose" => Box::new(goose::adapter(home, cfg)),
        "opencode" => Box::new(opencode::adapter(home, cfg)),
        // The two harnesses whose trigger lives in a program of their own (OpenClaw's scheduler;
        // Hermes's own config surface) implement this port on the same type that carries their
        // placement half — no wrapper, and the offline/live-harness knobs are theirs to declare.
        "openclaw" => Box::new(crate::OpenClaw::new(home.join(".openclaw"), cfg, run)),
        "hermes-agent" => Box::new(crate::Hermes::new(
            crate::registry::config_root(crate::registry::Root::HermesHome, home),
            crate::Hermes::resolve_accept_hooks(),
            cfg,
        )),
        _ => return None,
    })
}

/// The Claude Code trigger over an EXPLICIT config root — the injection-explicit constructor for a
/// caller that already knows the root, and the ONE way a suite builds this harness's trigger.
///
/// [`adapter_for_slug`] resolves the root through the registry, which honors `$CLAUDE_CONFIG_DIR`:
/// a test that took its trigger from there would write the DEVELOPER's `settings.json` whenever the
/// variable happens to be set, whatever temp home it passed. This takes the root instead, so no
/// ambient environment can reach it. It is the same spec and the same machinery
/// [`adapter_for_slug`] builds — one code path, one config shape.
#[must_use]
pub fn claude_code_at(root: PathBuf, cfg: &dyn ConfigStore) -> Box<dyn TriggerAdapter + '_> {
    Box::new(cc_hooks::JsonHooks::new(
        &crate::claude_code::SPEC,
        root,
        cfg,
    ))
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

    /// The disclosure's WORDS, pinned. A path prints as itself; a trigger with no path names the
    /// harness, admits it was not probed ("if armed" — proving it would mean running the harness),
    /// and says where the removal actually happens, so a person reading a preview is never
    /// surprised by what an apply reaches.
    #[test]
    fn an_artifact_renders_as_a_path_or_as_the_job_it_names() {
        assert_eq!(
            TriggerArtifact::Path(PathBuf::from("/home/me/.claude/settings.json")).to_string(),
            "/home/me/.claude/settings.json"
        );
        assert_eq!(
            TriggerArtifact::OutOfProcess {
                harness: "OpenClaw"
            }
            .to_string(),
            "the OpenClaw scheduled update job, if armed (removed through OpenClaw's scheduler)"
        );
        // The path row is the filter a path-typed surface applies; the other one has none to give.
        assert_eq!(
            TriggerArtifact::Path(PathBuf::from("/h/x")).path(),
            Some(Path::new("/h/x"))
        );
        assert_eq!(TriggerArtifact::OutOfProcess { harness: "X" }.path(), None);
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
