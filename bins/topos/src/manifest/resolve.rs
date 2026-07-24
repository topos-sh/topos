//! The layered resolution — "an agent here gets demand ∩ entitlement": combine every manifest
//! covering the working directory, NEAREST FIRST, deduped by item NAME.
//!
//! The layers, in resolution order (see the module doc): this folder's `topos.toml` →
//! ancestors' → the per-workspace PROFILES (server-stored; each connected session's delivery
//! answer arrives here as one ready-made layer) → the local personal manifest. A name claimed
//! by a nearer layer SHADOWS every broader mention; an EXCLUDE line claims a name the same way
//! (the one negative state) — so "why does this agent have X?" is always answered by ONE line
//! in ONE manifest, and "why not?" by no line, no entitlement, or an exclude you can read.

use std::path::PathBuf;

use crate::manifest::file::Manifest;
use crate::manifest::refs::{ParsedRef, entry_pin_error, parse_ref};

/// Which manifest a layer IS — the trust rail's "which manifest line asked for it".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LayerSource {
    /// A folder's `topos.toml` (this folder or an ancestor).
    Project { dir: PathBuf },
    /// The person's server-stored profile for one workspace (delivery-resolved).
    Profile { host: String, workspace: String },
    /// `~/.topos/topos.toml`.
    Personal,
}

impl LayerSource {
    /// The one-line label receipts and `status` print.
    pub(crate) fn label(&self) -> String {
        match self {
            LayerSource::Project { dir } => format!("{}/topos.toml", dir.display()),
            LayerSource::Profile { host, workspace } => {
                format!("your profile @ {host}/{workspace}")
            }
            LayerSource::Personal => "~/.topos/topos.toml".to_string(),
        }
    }
}

/// One layer as the resolver consumes it. Project/personal layers carry a parsed [`Manifest`];
/// profile layers arrive pre-resolved (the server already intersected demand with entitlement
/// and expanded channels), so they carry plain skill items.
#[derive(Debug, Clone)]
pub(crate) struct Layer {
    pub source: LayerSource,
    pub manifest: Manifest,
}

impl Layer {
    pub(crate) fn project(dir: PathBuf, manifest: Manifest) -> Self {
        Layer {
            source: LayerSource::Project { dir },
            manifest,
        }
    }

    pub(crate) fn personal(manifest: Manifest) -> Self {
        Layer {
            source: LayerSource::Personal,
            manifest,
        }
    }

    /// A profile layer from a session's delivered set: (name, canonical ref, pin) triples.
    pub(crate) fn profile(
        host: String,
        workspace: String,
        delivered: Vec<(String, String, Option<String>)>,
    ) -> Self {
        let manifest = Manifest {
            skills: delivered
                .into_iter()
                .map(|(_, reference, pin)| crate::manifest::file::ManifestEntry { reference, pin })
                .collect(),
            ..Manifest::default()
        };
        Layer {
            source: LayerSource::Profile { host, workspace },
            manifest,
        }
    }
}

/// Where a resolved item's bytes belong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedScope {
    /// Materialize inside THIS project (its harness dirs, kept out of commits).
    Project { dir: PathBuf },
    /// Materialize in the global home harness dirs.
    Person,
}

/// What kind of thing one resolved line names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemKind {
    Skill,
    /// A channel reference — the reconcile expands it against the session's channel index.
    Channel,
    /// An external GitHub origin (pinned by default).
    GitHub,
    /// A local folder (personal-manifest solo mode, or a pre-publish project skill).
    LocalPath,
}

/// One resolved line: the name, the winning reference, and its provenance.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedItem {
    /// The dedupe key (the reference's last segment).
    pub name: String,
    /// The winning canonical reference.
    pub reference: String,
    pub parsed: ParsedRef,
    pub kind: ItemKind,
    pub pin: Option<String>,
    pub scope: ResolvedScope,
    /// Which manifest asked for it — the trust rail's first half.
    pub source: LayerSource,
    /// Broader mentions this line shadows (rendered by `status`, never acted on).
    pub shadowed_from: Vec<LayerSource>,
}

/// A name a manifest EXCLUDED (with which layer said so) — `status` renders these.
#[derive(Debug, Clone)]
pub(crate) struct ExcludedItem {
    pub name: String,
    pub by: LayerSource,
    /// Broader mentions the exclude shadowed (what WOULD have been delivered).
    pub shadowed_from: Vec<LayerSource>,
}

/// A manifest line the grammar refused (surfaced honestly; resolution continues without it).
#[derive(Debug, Clone)]
pub(crate) struct ManifestIssue {
    pub source: LayerSource,
    pub reference: String,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Resolution {
    pub items: Vec<ResolvedItem>,
    pub excluded: Vec<ExcludedItem>,
    pub issues: Vec<ManifestIssue>,
}

/// The exclude key an `exclude = […]` line claims: the last path segment (so an exclude by
/// bare name and by full reference both work — the receipt writes the full form).
fn exclude_name(reference: &str) -> &str {
    reference
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(reference)
}

/// Resolve the layer chain (already ordered nearest-first) into the delivered set. Pure:
/// no I/O, no network — profile layers were materialized by the caller.
pub(crate) fn resolve_layers(layers: &[Layer]) -> Resolution {
    let mut out = Resolution::default();
    // name → index into out.items / out.excluded (the FIRST claim wins; later ones shadow).
    let mut claimed: std::collections::HashMap<String, Claim> = std::collections::HashMap::new();

    enum Claim {
        Item(usize),
        Excluded(usize),
    }

    for layer in layers {
        let scope = match &layer.source {
            LayerSource::Project { dir } => ResolvedScope::Project { dir: dir.clone() },
            LayerSource::Profile { .. } | LayerSource::Personal => ResolvedScope::Person,
        };
        // Excludes claim their names FIRST within a layer: `remove` writes an exclude next to
        // nothing else, and a same-layer include+exclude collision resolves toward the exclude
        // (the more recent, deliberate act).
        for reference in &layer.manifest.exclude {
            let name = exclude_name(reference).to_string();
            match claimed.get(&name) {
                Some(Claim::Item(i)) => {
                    // A NEARER layer included it — the include shadows this broader exclude.
                    out.items[*i].shadowed_from.push(layer.source.clone());
                }
                Some(Claim::Excluded(i)) => {
                    out.excluded[*i].shadowed_from.push(layer.source.clone());
                }
                None => {
                    out.excluded.push(ExcludedItem {
                        name: name.clone(),
                        by: layer.source.clone(),
                        shadowed_from: Vec::new(),
                    });
                    claimed.insert(name, Claim::Excluded(out.excluded.len() - 1));
                }
            }
        }
        for (entries, kind_hint) in [
            (&layer.manifest.skills, None),
            (&layer.manifest.channels, Some(ItemKind::Channel)),
        ] {
            for entry in entries {
                let parsed = match parse_ref(&entry.reference) {
                    Ok(p) => p,
                    Err(e) => {
                        out.issues.push(ManifestIssue {
                            source: layer.source.clone(),
                            reference: entry.reference.clone(),
                            message: e.message,
                        });
                        continue;
                    }
                };
                // The entry VALUE is a pin spec too — validated with the same per-kind rules
                // the `@pin` spelling gets (a handwritten manifest is no back door).
                if let Some(pin) = &entry.pin
                    && let Err(e) = entry_pin_error(&parsed, pin)
                {
                    out.issues.push(ManifestIssue {
                        source: layer.source.clone(),
                        reference: entry.reference.clone(),
                        message: e.message,
                    });
                    continue;
                }
                let kind = kind_hint.unwrap_or(match &parsed {
                    ParsedRef::Channel { .. } => ItemKind::Channel,
                    ParsedRef::GitHub { .. } => ItemKind::GitHub,
                    ParsedRef::LocalPath { .. } => ItemKind::LocalPath,
                    ParsedRef::Bare { .. } | ParsedRef::Skill { .. } => ItemKind::Skill,
                });
                let name = parsed.item_name().to_string();
                match claimed.get(&name) {
                    Some(Claim::Item(i)) => {
                        out.items[*i].shadowed_from.push(layer.source.clone());
                    }
                    Some(Claim::Excluded(i)) => {
                        // A NEARER exclude beats this broader provision (project excludes beat
                        // the profile inside the project).
                        out.excluded[*i].shadowed_from.push(layer.source.clone());
                    }
                    None => {
                        out.items.push(ResolvedItem {
                            name: name.clone(),
                            reference: entry.reference.clone(),
                            pin: entry.pin.clone().or_else(|| parsed.pin().map(String::from)),
                            parsed,
                            kind,
                            scope: scope.clone(),
                            source: layer.source.clone(),
                            shadowed_from: Vec::new(),
                        });
                        claimed.insert(name, Claim::Item(out.items.len() - 1));
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::file::ManifestEntry;

    fn manifest(skills: &[(&str, Option<&str>)], exclude: &[&str]) -> Manifest {
        Manifest {
            skills: skills
                .iter()
                .map(|(r, p)| ManifestEntry {
                    reference: (*r).to_string(),
                    pin: p.map(String::from),
                })
                .collect(),
            exclude: exclude.iter().map(|s| (*s).to_string()).collect(),
            ..Manifest::default()
        }
    }

    fn dir(p: &str) -> PathBuf {
        PathBuf::from(p)
    }

    #[test]
    fn nearest_manifest_wins_on_a_name() {
        let pin = "1".repeat(64);
        let layers = vec![
            Layer::project(
                dir("/repo/api"),
                manifest(&[("topos.sh/acme/deploy", Some(&pin))], &[]),
            ),
            Layer::project(
                dir("/repo"),
                manifest(&[("topos.sh/acme/deploy", None)], &[]),
            ),
            Layer::profile(
                "topos.sh".into(),
                "acme".into(),
                vec![("deploy".into(), "topos.sh/acme/deploy".into(), None)],
            ),
        ];
        let r = resolve_layers(&layers);
        assert_eq!(r.items.len(), 1);
        let item = &r.items[0];
        assert_eq!(item.name, "deploy");
        // The nearest layer's pin wins; the two broader mentions are recorded as shadowed.
        assert_eq!(item.pin.as_deref(), Some(pin.as_str()));
        assert_eq!(
            item.scope,
            ResolvedScope::Project {
                dir: dir("/repo/api")
            }
        );
        assert_eq!(item.shadowed_from.len(), 2);
    }

    #[test]
    fn a_project_exclude_beats_the_profile_inside_the_project() {
        let layers = vec![
            Layer::project(dir("/repo"), manifest(&[], &["topos.sh/acme/noisy"])),
            Layer::profile(
                "topos.sh".into(),
                "acme".into(),
                vec![
                    ("noisy".into(), "topos.sh/acme/noisy".into(), None),
                    ("keep".into(), "topos.sh/acme/keep".into(), None),
                ],
            ),
        ];
        let r = resolve_layers(&layers);
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].name, "keep");
        assert_eq!(r.excluded.len(), 1);
        assert_eq!(r.excluded[0].name, "noisy");
        // The exclude names what it withheld (status renders it).
        assert_eq!(r.excluded[0].shadowed_from.len(), 1);
    }

    #[test]
    fn a_nearer_include_shadows_a_broader_exclude() {
        let layers = vec![
            Layer::project(
                dir("/repo/api"),
                manifest(&[("topos.sh/acme/x", None)], &[]),
            ),
            Layer::project(dir("/repo"), manifest(&[], &["topos.sh/acme/x"])),
        ];
        let r = resolve_layers(&layers);
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].shadowed_from.len(), 1);
        assert!(r.excluded.is_empty());
    }

    #[test]
    fn scopes_follow_the_layer_kind() {
        let layers = vec![
            Layer::project(dir("/repo"), manifest(&[("topos.sh/acme/proj", None)], &[])),
            Layer::profile(
                "topos.sh".into(),
                "acme".into(),
                vec![("prof".into(), "topos.sh/acme/prof".into(), None)],
            ),
            Layer::personal(manifest(&[("./local-skill", None)], &[])),
        ];
        let r = resolve_layers(&layers);
        let by_name = |n: &str| r.items.iter().find(|i| i.name == n).unwrap();
        assert_eq!(
            by_name("proj").scope,
            ResolvedScope::Project { dir: dir("/repo") }
        );
        assert_eq!(by_name("prof").scope, ResolvedScope::Person);
        assert_eq!(by_name("local-skill").scope, ResolvedScope::Person);
        assert_eq!(by_name("local-skill").kind, ItemKind::LocalPath);
    }

    #[test]
    fn channel_entries_resolve_as_channels() {
        let mut m = manifest(&[], &[]);
        m.channels.push(ManifestEntry {
            reference: "topos.sh/acme/channels/backend".into(),
            pin: None,
        });
        let layers = vec![Layer::project(dir("/repo"), m)];
        let r = resolve_layers(&layers);
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].kind, ItemKind::Channel);
        assert_eq!(r.items[0].name, "backend");
    }

    #[test]
    fn a_bad_entry_pin_is_an_issue_never_a_stop() {
        let layers = vec![Layer::project(
            dir("/repo"),
            manifest(
                &[
                    // A workspace pin must be the FULL digest.
                    ("topos.sh/acme/short-pinned", Some("abc1234")),
                    // A GitHub pin is commit-shaped, never the 64-hex digest length.
                    ("github.com/o/r", Some(&"1".repeat(64))),
                    ("topos.sh/acme/fine", None),
                ],
                &[],
            ),
        )];
        let r = resolve_layers(&layers);
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].name, "fine");
        assert_eq!(r.issues.len(), 2);
        // A channel entry takes no pin at all.
        let mut m = manifest(&[], &[]);
        m.channels.push(ManifestEntry {
            reference: "topos.sh/acme/channels/backend".into(),
            pin: Some("1".repeat(64)),
        });
        let r = resolve_layers(&[Layer::project(dir("/repo"), m)]);
        assert!(r.items.is_empty());
        assert_eq!(r.issues.len(), 1);
    }

    #[test]
    fn a_bad_reference_is_an_issue_never_a_stop() {
        let layers = vec![Layer::project(
            dir("/repo"),
            manifest(&[("#nope", None), ("topos.sh/acme/fine", None)], &[]),
        )];
        let r = resolve_layers(&layers);
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.issues.len(), 1);
        assert!(r.issues[0].message.contains("channels"));
    }

    #[test]
    fn a_bare_exclude_matches_a_qualified_provision() {
        let layers = vec![
            Layer::project(dir("/repo"), manifest(&[], &["noisy"])),
            Layer::profile(
                "topos.sh".into(),
                "acme".into(),
                vec![("noisy".into(), "topos.sh/acme/noisy".into(), None)],
            ),
        ];
        let r = resolve_layers(&layers);
        assert!(r.items.is_empty());
        assert_eq!(r.excluded.len(), 1);
    }
}
