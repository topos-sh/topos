//! **Shared-dir coverage** — which harnesses read the cross-client convention dir `~/.agents/skills`.
//!
//! Several agent CLIs have converged on one user-scope skills dir (`<home>/.agents/skills`), so a skill
//! placed THERE reaches every one of them with a single copy. Whether a given harness actually READS
//! that dir is a per-harness fact with **provenance**: some claims were verified against a live build
//! ([`SharedDirSupport::Probed`]), some rest on vendor documentation ([`SharedDirSupport::Docs`]), and
//! everything else is [`SharedDirSupport::Unknown`] — treated as *not* covered (fail closed: a skill
//! must never be assumed delivered to a harness on no evidence).
//!
//! The claim lives ON the harness's [`registry`] row ([`registry::KnownHarness::shared_dir_claim`]),
//! each with its evidence in a comment, so a fresh probe result is a one-line edit. A row carrying no
//! claim of its own falls through to the derivation rule: a row whose USER dirs include the literal home
//! `.agents/skills` spec is itself a docs-level claim (`Docs(true)`), which keeps registry re-syncs in
//! sync automatically.

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

/// The canonical raw spec string a registry row carries when its user skills dir IS the shared dir —
/// the derivation rule's needle (see [`registry::DirSpec::raw`] for the encoding).
const SHARED_USER_SPEC: &str = "home/.agents/skills";

/// The shared-dir coverage claim for a harness slug: the row's own claim, else the registry derivation
/// (a user dir at the literal home `.agents/skills` ⇒ `Docs(true)`), else `Unknown`.
#[must_use]
pub fn shared_dir_support(slug: &str) -> SharedDirSupport {
    let Some(harness) = registry::known_harness(slug) else {
        return SharedDirSupport::Unknown;
    };
    match harness.shared_dir_claim() {
        SharedDirSupport::Unknown => {
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
        claim => claim,
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

    /// A row's own claim wins over the derivation and carries its provenance.
    #[test]
    fn a_rows_claim_wins_and_carries_its_provenance() {
        // The live probes (positive and negative; `cline`/`crush` upgrade or establish what the
        // row's own dirs could not derive).
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
        // claim on its row is the whole answer — and it must NOT read as covered.
        assert!(!shared_dir_support("codex").covered());
    }

    #[test]
    fn registry_rows_with_the_shared_user_dir_derive_docs_true() {
        // These rows carry `home/.agents/skills` as their user dir upstream — the registry claim is
        // itself a docs-level source, so they derive Docs(true) carrying no claim of their own.
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
