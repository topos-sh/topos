//! **Shared-dir coverage** — which harnesses read the cross-client convention dir `~/.agents/skills`.
//!
//! Several agent CLIs have converged on one user-scope skills dir (`<home>/.agents/skills`), so a skill
//! placed THERE reaches every one of them with a single copy. Whether a given harness actually READS
//! that dir is a per-harness fact with **provenance**: some claims were verified against a live build
//! ([`SharedDirSupport::Probed`]), some rest on vendor documentation ([`SharedDirSupport::Docs`]), and
//! everything else is [`SharedDirSupport::Unknown`] — treated as *not* covered (fail closed: a skill
//! must never be assumed delivered to a harness on no evidence).
//!
//! Two sources feed [`shared_dir_support`], override first:
//! 1. the small override table below — one row per line, each commented with its evidence source,
//!    so a fresh probe result is a one-line edit;
//! 2. the derivation rule — a [`registry`] row whose USER dirs include the literal home
//!    `.agents/skills` spec is itself a docs-level claim (`Docs(true)`), and this stays in sync with
//!    registry re-syncs automatically.

use std::path::{Path, PathBuf};

use crate::registry;

/// Whether a harness reads the shared `~/.agents/skills` dir, with the claim's provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedDirSupport {
    /// Verified against a live build (a real install probed reading — or provably NOT reading — the
    /// shared dir).
    Probed(bool),
    /// Vendor docs, or the upstream registry's own directory claim — plausible, not live-verified.
    Docs(bool),
    /// No evidence either way — treated as NOT covered.
    Unknown,
}

impl SharedDirSupport {
    /// Whether the harness is covered by the shared dir (a `true` claim at either provenance level).
    #[must_use]
    pub fn covered(self) -> bool {
        matches!(
            self,
            SharedDirSupport::Probed(true) | SharedDirSupport::Docs(true)
        )
    }

    /// Whether the coverage claim is docs-level only (an honest disclosure hook for surfaces that
    /// name the shared dir: "covered per vendor docs" reads differently from "probed live").
    #[must_use]
    pub fn docs_level(self) -> bool {
        matches!(self, SharedDirSupport::Docs(true))
    }
}

/// The per-harness overrides — one row per line, each commented with its evidence source, so fresh
/// probe results are trivial to fold in. An override WINS over the registry derivation.
const OVERRIDES: &[(&str, SharedDirSupport)] = &[
    // Verified against a live containerized install of openclaw@2026.7.1 on 2026-07-16:
    // `~/.agents/skills` is a recognized skills root, higher precedence than `~/.openclaw/skills`.
    ("openclaw", SharedDirSupport::Probed(true)),
    // Verified against a live codex-cli 0.144.4 binary on 2026-07-16: no `.agents/skills` path
    // literal exists in the build; its only user skills root is `$CODEX_HOME/skills`.
    ("codex", SharedDirSupport::Probed(false)),
    // Vendor manual lists `~/.agents/skills` among its skills dirs (closed source — docs only).
    ("amp", SharedDirSupport::Docs(true)),
    // Vendor docs list `~/.agents/skills`.
    ("gemini-cli", SharedDirSupport::Docs(true)),
    // Vendor docs list `~/.agents/skills`.
    ("github-copilot", SharedDirSupport::Docs(true)),
    // Verified LIVE against goose 1.43.0 in a container on 2026-07-16: a skill placed at
    // `~/.agents/skills/<name>` appears in `goose skills list` (and `~/.agents/skills` is the
    // build's writable global skills dir).
    ("goose", SharedDirSupport::Probed(true)),
    // Verified against a live containerized opencode-ai 1.18.3 on 2026-07-16: the binary's own
    // help text names `~/.agents/skills/<name>/SKILL.md` as an auto-loaded external skills dir.
    ("opencode", SharedDirSupport::Probed(true)),
    // Verified against cline 3.0.43 source on 2026-07-16: `~/.agents/skills` is in the global
    // skills search paths (upgrades the registry-derived docs-level claim).
    ("cline", SharedDirSupport::Probed(true)),
    // Verified against crush v0.85.0 source on 2026-07-16: `~/.agents/skills` is in the global
    // skills dirs ("Per the Agent Skills spec"); its registry row alone would derive nothing.
    ("crush", SharedDirSupport::Probed(true)),
];

/// The canonical raw spec string a registry row carries when its user skills dir IS the shared dir —
/// the derivation rule's needle (see [`registry::KnownHarness::user_dir_specs`] for the encoding).
const SHARED_USER_SPEC: &str = "home/.agents/skills";

/// The shared-dir coverage claim for a harness slug. Override first, then the registry derivation
/// (a user dir at the literal home `.agents/skills` ⇒ `Docs(true)`), else `Unknown`.
#[must_use]
pub fn shared_dir_support(slug: &str) -> SharedDirSupport {
    if let Some((_, support)) = OVERRIDES.iter().find(|(s, _)| *s == slug) {
        return *support;
    }
    let Some(harness) = registry::known_harnesses().iter().find(|h| h.slug == slug) else {
        return SharedDirSupport::Unknown;
    };
    if harness
        .user_dir_specs()
        .iter()
        .any(|s| s == SHARED_USER_SPEC)
    {
        SharedDirSupport::Docs(true)
    } else {
        SharedDirSupport::Unknown
    }
}

/// The shared cross-client skills dir under `home` — `<home>/.agents/skills`.
#[must_use]
pub fn shared_skills_dir(home: &Path) -> PathBuf {
    home.join(".agents").join("skills")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_win_and_carry_their_provenance() {
        // The live probes (positive and negative; `cline`/`crush` upgrade or establish what the
        // registry row alone could not).
        for slug in ["openclaw", "goose", "opencode", "cline", "crush"] {
            assert_eq!(
                shared_dir_support(slug),
                SharedDirSupport::Probed(true),
                "{slug}"
            );
        }
        assert_eq!(shared_dir_support("codex"), SharedDirSupport::Probed(false));
        // The docs-level overrides.
        for slug in ["amp", "gemini-cli", "github-copilot"] {
            assert_eq!(
                shared_dir_support(slug),
                SharedDirSupport::Docs(true),
                "{slug}"
            );
        }
        // `codex` derives nothing (its user dir is `$CODEX_HOME/skills`), so the Probed(false)
        // override is the whole claim — and it must NOT read as covered.
        assert!(!shared_dir_support("codex").covered());
    }

    #[test]
    fn registry_rows_with_the_shared_user_dir_derive_docs_true() {
        // These rows carry `home/.agents/skills` as their user dir upstream — the registry claim is
        // itself a docs-level source, so they derive Docs(true) with no override row.
        for slug in ["zed", "dexto", "kimi-code-cli", "loaf", "warp"] {
            let support = shared_dir_support(slug);
            assert_eq!(support, SharedDirSupport::Docs(true), "{slug}");
            assert!(support.covered() && support.docs_level(), "{slug}");
        }
    }

    #[test]
    fn everything_else_is_unknown_and_uncovered() {
        // A harness with its own private skills dir and no override carries no evidence.
        for slug in ["claude-code", "cursor", "hermes-agent", "windsurf"] {
            assert_eq!(
                shared_dir_support(slug),
                SharedDirSupport::Unknown,
                "{slug}"
            );
            assert!(!shared_dir_support(slug).covered(), "{slug}");
        }
        // An unknown slug is Unknown too (never a panic).
        assert_eq!(
            shared_dir_support("not-a-harness"),
            SharedDirSupport::Unknown
        );
    }

    #[test]
    fn shared_skills_dir_is_home_agents_skills() {
        assert_eq!(
            shared_skills_dir(Path::new("/home/u")),
            Path::new("/home/u/.agents/skills")
        );
    }
}
