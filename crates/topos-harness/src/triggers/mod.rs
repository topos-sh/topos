//! `triggers` — breadth auto-update triggers: one [`TriggerAdapter`] per additional registry-slug
//! harness, all running the ONE byte-stable sweep (`topos update --quiet`, which self-throttles
//! client-side, so session-shaped re-fires are cheap).
//!
//! The three fully-adapted harnesses (Claude Code, OpenClaw, Hermes) keep their own
//! [`HarnessAdapter`](crate::HarnessAdapter) reports; this module is the breadth surface — a
//! trigger (un)install + health probe for nine more harnesses, keyed by their
//! [`registry`](crate::registry) slugs. Two shared bases carry the machinery:
//!
//! - `cc_hooks` — the JSON-config-merge family (Claude-Code-shaped hooks registered in a shared
//!   strict-JSON config file): `gemini-cli`, `cursor`, `droid`.
//! - `file_drop` — one topos-owned file at a harness-defined path: `github-copilot`, `opencode`,
//!   `goose`, `amp`, `cline`.
//! - `codex` is special: its config is TOML (no TOML dependency exists in this crate), so it is a
//!   line-anchored merge mirroring the Hermes YAML discipline — provable shapes only, fail-closed
//!   on everything else.
//!
//! Every adapter here mirrors the big-three idiom: content-blind, an injected home (never the
//! real `~`), durable writes through the [`ConfigStore`] port, sentinel/marker-keyed ownership,
//! adopt-or-leave on foreign artifacts, and fail-closed (`Degraded`, ZERO writes) on any shape it
//! cannot prove. Honesty is structural: `Active` is claimed only on the per-instance evidence
//! documented in each instance module; a registration whose harness gates hooks behind its own
//! consent (and whose consent store is not readable evidence) reports `Inactive` with the
//! [`CurrencyKind::ExplicitPullOnly`] floor and a note naming the consent step still owed. No
//! adapter ever WRITES another program's trust/consent state — at most it reads it, fail-closed,
//! as evidence.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, TriggerState};

use crate::ConfigStore;

mod amp;
mod cc_hooks;
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

/// The ONE shell-string sweep line every shell surface registers: the guarded sweep + the
/// trailing ownership sentinel (inert under `sh -c`), exactly the Claude Code spelling.
pub(crate) const SHELL_SWEEP_LINE: &str =
    "command -v topos >/dev/null 2>&1 && topos update --quiet || true  # topos:currency";

/// The plain argv sweep for non-shell / code surfaces: plugin code runs this and swallows
/// failures itself (no shell guard is possible there, and none is needed — the plugin's own
/// try/catch is the exit-0 tail's analog).
pub(crate) const PLAIN_SWEEP: &str = "topos update --quiet";

/// One trigger (un)install outcome for a registry-slug harness. The big-three adapters keep
/// their own [`HarnessAdapter`](crate::HarnessAdapter) `TriggerReport`s; this is the breadth
/// surface's receipt.
#[derive(Debug, Clone)]
pub struct TriggerOutcome {
    /// The registry slug (see [`registry`](crate::registry)).
    pub slug: &'static str,
    /// Honest trigger labeling: what fires when `state` is [`TriggerState::Active`]; the
    /// [`CurrencyKind::ExplicitPullOnly`] floor on every other state.
    pub kind: CurrencyKind,
    pub state: TriggerState,
    /// The file this call actually wrote (or unlinked) — `None` on a true no-op.
    pub touched_path: Option<String>,
    /// The structured marker identity (topos + slug + schema version).
    pub marker_id: String,
    /// A short human note carried to the receipt: the consent step still owed, or the evidence
    /// level ("vendor docs, unverified") — `None` when nothing needs saying.
    pub note: Option<String>,
}

/// The auto-update-trigger port for one registry-slug harness: idempotent (un)install of the one
/// sweep trigger, plus a provable-presence health probe.
pub trait TriggerAdapter {
    /// The registry slug this adapter serves.
    fn slug(&self) -> &'static str;
    /// Idempotently install the auto-update trigger — a rerun over an already-canonical artifact
    /// writes nothing; anything unprovable degrades with zero writes.
    fn install(&self) -> TriggerOutcome;
    /// Surgically remove OUR trigger artifact (sentinel/marker-confirmed only — a foreign
    /// artifact is never touched); idempotent.
    fn remove(&self) -> TriggerOutcome;
    /// Provable presence of OUR trigger artifact right now (the health probe). Anything
    /// unprovable answers `false` — presence is never claimed on faith.
    fn present(&self) -> bool;
}

/// The registry slugs with trigger support here (registry-table order), for the integrator's
/// arming sweep. Every other slug is placement-only — [`adapter_for_slug`] answers `None`.
#[must_use]
pub fn supported_slugs() -> &'static [&'static str] {
    &[
        "amp",
        "cline",
        "codex",
        "cursor",
        "droid",
        "gemini-cli",
        "github-copilot",
        "goose",
        "opencode",
    ]
}

/// Construct the trigger adapter for a registry slug, over an injected home + the [`ConfigStore`]
/// port. `home` is the USER home dir; each adapter resolves its own harness root under it,
/// honoring the harness's env override (`$CODEX_HOME`, `$XDG_CONFIG_HOME`) the way the registry
/// does. `None` = no trigger support for that slug (a placement-only harness). Tests construct
/// the adapters over fully-injected roots instead, so no suite depends on the real environment.
#[must_use]
pub fn adapter_for_slug<'a>(
    slug: &str,
    home: &Path,
    cfg: &'a dyn ConfigStore,
) -> Option<Box<dyn TriggerAdapter + 'a>> {
    Some(match slug {
        "amp" => Box::new(amp::adapter(home, cfg)),
        "cline" => Box::new(cline::adapter(home, cfg)),
        "codex" => Box::new(codex::adapter(home, cfg)),
        "cursor" => Box::new(cursor::adapter(home, cfg)),
        "droid" => Box::new(droid::adapter(home, cfg)),
        "gemini-cli" => Box::new(gemini_cli::adapter(home, cfg)),
        "github-copilot" => Box::new(github_copilot::adapter(home, cfg)),
        "goose" => Box::new(goose::adapter(home, cfg)),
        "opencode" => Box::new(opencode::adapter(home, cfg)),
        _ => return None,
    })
}

/// Build a [`TriggerOutcome`] with the honest kind rule applied: only an `Active` state carries
/// the instance's live trigger kind; every other state advertises just the guaranteed floor —
/// an explicit `topos update`.
pub(crate) fn outcome(
    slug: &'static str,
    live_kind: CurrencyKind,
    state: TriggerState,
    touched_path: Option<String>,
    marker_id: &str,
    note: Option<&str>,
) -> TriggerOutcome {
    TriggerOutcome {
        slug,
        kind: if state == TriggerState::Active {
            live_kind
        } else {
            CurrencyKind::ExplicitPullOnly
        },
        state,
        touched_path,
        marker_id: marker_id.to_owned(),
        note: note.map(str::to_owned),
    }
}

/// Read a `$VAR` home override — trimmed, non-empty — as a path, else `None` (the same
/// resolution rule the registry's baked table uses). The ONE place these adapters read the real
/// environment; every test constructs adapters over injected roots instead.
pub(crate) fn env_override(var: &str) -> Option<PathBuf> {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `$XDG_CONFIG_HOME` else `home/.config` — the config-home root the XDG-rooted harnesses
/// (`opencode`, `amp`, `goose`'s own config) resolve under, matching the registry's rule.
pub(crate) fn resolve_config_home(home: &Path) -> PathBuf {
    env_override("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"))
}

#[cfg(test)]
mod tests {
    use super::testutil::MemConfig;
    use super::*;

    #[test]
    fn the_sweep_consts_compose() {
        assert_eq!(
            SHELL_SWEEP_LINE,
            format!("{GUARDED_SWEEP}  {SENTINEL}"),
            "the shell line is the guarded sweep + two spaces + the sentinel — the Claude Code spelling"
        );
        assert!(GUARDED_SWEEP.contains(PLAIN_SWEEP));
        assert!(GUARDED_SWEEP.starts_with("command -v topos"));
        assert!(GUARDED_SWEEP.ends_with("|| true"));
    }

    #[test]
    fn adapter_for_slug_covers_exactly_the_supported_slugs() {
        let cfg = MemConfig::default();
        let home = std::path::PathBuf::from("/no-such-home");
        for slug in supported_slugs() {
            let adapter = adapter_for_slug(slug, &home, &cfg)
                .unwrap_or_else(|| panic!("{slug} is supported"));
            assert_eq!(adapter.slug(), *slug);
            assert!(
                !adapter.present(),
                "{slug}: nothing on an empty store is ever claimed present"
            );
        }
        // Placement-only (or fully-adapted) slugs get no breadth trigger adapter.
        for slug in ["claude-code", "openclaw", "hermes-agent", "zed", "warp", ""] {
            assert!(adapter_for_slug(slug, &home, &cfg).is_none(), "{slug}");
        }
    }

    #[test]
    fn outcome_advertises_only_the_floor_when_not_active() {
        use topos_types::{CurrencyKind, TriggerState};
        for state in [
            TriggerState::Inactive,
            TriggerState::Degraded,
            TriggerState::AlreadyPresentUnmanaged,
        ] {
            let out = outcome("cline", CurrencyKind::SessionStart, state, None, "m", None);
            assert_eq!(out.kind, CurrencyKind::ExplicitPullOnly, "{state:?}");
        }
        let live = outcome(
            "cline",
            CurrencyKind::SessionStart,
            TriggerState::Active,
            None,
            "m",
            None,
        );
        assert_eq!(live.kind, CurrencyKind::SessionStart);
    }
}
