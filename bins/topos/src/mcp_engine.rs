//! The per-scope MCP CONVERGENCE engine — how a `kind = "mcp"` bundle's `server.json` becomes (and
//! stops being) an entry in each detected agent's own MCP config.
//!
//! The pure placement math lives in `topos_harness::mcp` (bytes in → an [`EditPlan`] out, one
//! driver per dialect); THIS module owns everything stateful around it, per scope:
//!
//! - the demand set (what the reconcile resolved this run — see [`McpDemand`]),
//! - `server.json` parsing with a fail-closed re-check of the publish gate (a secret /
//!   templated / value-less header the gate should have refused is never placed, only warned
//!   about),
//! - the [`crate::mcp_ledger`] discipline: key minting, the `key → fingerprint` prior map, the
//!   intent journal around EVERY config write, and crash recovery at converge start,
//! - the per-harness surface: the descriptor table joined onto detection (a harness engages only
//!   when its slug is detected OR its config file already exists), the person/project surface
//!   split (a project scope NEVER falls back to a user surface), and the project containment
//!   proof (the [`crate::placement::within_project`] rail — refused + disclosed, never
//!   redirected),
//! - removal convergence: ledger entries whose bundle is no longer demanded leave through the
//!   drivers' prior-matched removal; DRIFTED entries are left byte-identical and disclosed; a
//!   file topos created and still wholly owned is deleted when its last entry leaves — behind a
//!   double belt: `owns_file` is invalidated at the FIRST sighting of any content beyond our
//!   managed entries, and the delete re-reads the post-image and fires only when it is
//!   structurally empty (a plain user entry a driver's states cannot see is never destroyed),
//! - the wholly-topos-owned Claude Code plugin dir (rendered whole — never patched).
//!
//! The engine is OFFLINE by construction: demands carry the stored `server.json` bytes, so a dead
//! network still heals config files from the store + ledger.
//!
//! Wire states (an OPEN vocabulary, kept small): `current` (placed / updated / already there) ·
//! `drifted` (hand-edited since topos wrote it — untouched) · `not-supported` (withheld:
//! capability or no surface at this scope) · `unprovable` (the surface cannot be safely edited) ·
//! `conflicting` (the desired config key is occupied by an entry topos does not own) · `removed`
//! (removal receipts only).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;
use topos_harness::mcp::{
    self, AuthHint, EditPlan, EntryState, McpDialect, McpEntry, McpHarness, plugin_dir,
};
use topos_types::results::McpAgentState;

use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::mcp_ledger::{self, LedgerEntry, McpLedger, PendingIntent, placement_key};
use crate::sidecar::Layout;

/// One demanded MCP bundle in one scope — everything the engine needs, resolved by the reconcile.
#[derive(Debug, Clone)]
pub(crate) struct McpDemand {
    /// The bundle identity the ledger keys on (the workspace skill id; `local:<name>` for an
    /// untracked local row).
    pub bundle_id: String,
    /// The catalog / row name (the key-mint ingredient and the receipt name).
    pub name: String,
    /// The workspace address slug for a workspace bundle (`None` = a local row) — the key-mint
    /// namespace.
    pub workspace_slug: Option<String>,
    /// The stored version the `server.json` bytes came from (ledger provenance).
    pub version_id: String,
    /// The bundle's `server.json` bytes, read from the scope store's current tree (or the local
    /// row's dir).
    pub server_json: Vec<u8>,
    /// Registry slugs this demand is narrowed to (row `harness = [...]` beating
    /// `[defaults.mcp]`); empty = every MCP-capable harness.
    pub harness_filter: Vec<String>,
}

/// The scope's I/O: the fs seam, the scope store (the ledger's home), and the machine roots the
/// surfaces resolve against.
pub(crate) struct ScopeIo<'a> {
    pub fs: &'a dyn FsOps,
    /// The scope's store layout — where `state/mcp_ledger.json` lives.
    pub layout: &'a Layout,
    /// The machine home (user surfaces resolve under it).
    pub home: PathBuf,
    /// `Some` = the PROJECT scope: surfaces are the project-relative ones, containment-proven.
    pub project_root: Option<PathBuf>,
}

/// One bundle's per-agent outcome this converge.
#[derive(Debug)]
pub(crate) struct BundleStates {
    pub bundle_id: String,
    pub name: String,
    pub states: Vec<McpAgentState>,
}

/// One removed placement (removal convergence / the `remove` verb's inline converge).
#[derive(Debug)]
pub(crate) struct RemovedEntry {
    /// The bundle whose entry left — the `remove` verb matches its receipts by it; the sweep's
    /// disclosure lines carry the file + agent instead (the id is opaque to a person).
    #[allow(dead_code)]
    pub bundle_id: String,
    pub state: McpAgentState,
}

/// A converge's whole answer.
#[derive(Debug, Default)]
pub(crate) struct ConvergeOutcome {
    pub bundles: Vec<BundleStates>,
    pub removed: Vec<RemovedEntry>,
    /// Honest per-bundle / per-surface failure lines (the sweep's warning channel).
    pub warnings: Vec<String>,
}

// =================================================================================================
// server.json parsing — the fail-closed re-check of the publish gate.
// =================================================================================================

/// What one `server.json` resolves to for placement.
struct ParsedServer {
    url: String,
    headers: Vec<(String, String)>,
    auth: AuthHint,
}

/// Parse a bundle's `server.json`: the FIRST `remotes[]` entry with `type == "streamable-http"`,
/// its url, its LITERAL headers, and the `_meta["sh.topos/auth"]` hint. Anything the publish gate
/// should have refused — a secret / templated / variable / value-less header — fails the WHOLE
/// demand closed (never place a suspect entry).
fn parse_server_json(bytes: &[u8]) -> Result<ParsedServer, String> {
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("server.json is not valid JSON ({e})"))?;
    // A non-empty `packages[]` is a LOCALLY-RUN server — out of scope for a shared bundle, and
    // something the publish gate refuses outright; a stored copy carrying one is suspect.
    if root
        .get("packages")
        .and_then(Value::as_array)
        .is_some_and(|p| !p.is_empty())
    {
        return Err("server.json carries local packages[] — not a shareable remote server".into());
    }
    let remotes = root
        .get("remotes")
        .and_then(Value::as_array)
        .ok_or_else(|| "server.json carries no remotes[] list".to_owned())?;
    // The FIRST remote with `type == "streamable-http"` AND a url — ONE predicate, the same
    // pick the gate makes (`crate::mcp_validate`), so the engine can never resolve a different
    // remote than the one the gate approved.
    let remote = remotes
        .iter()
        .find(|r| {
            r.get("type").and_then(Value::as_str) == Some("streamable-http")
                && r.get("url").and_then(Value::as_str).is_some()
        })
        .ok_or_else(|| "server.json carries no streamable-http remote with a url".to_owned())?;
    let url = remote
        .get("url")
        .and_then(Value::as_str)
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| "the streamable-http remote carries no url".to_owned())?
        .to_owned();
    let mut headers = Vec::new();
    if let Some(list) = remote.get("headers").and_then(Value::as_array) {
        for h in list {
            let name = h
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.trim().is_empty())
                .ok_or_else(|| "a header entry carries no name".to_owned())?;
            // The publish gate refused secret / variable / value-less headers. RE-CHECK here,
            // fail-closed: a suspect header fails the whole demand rather than placing a header
            // whose value the gate never validated as a shareable literal. (`isRequired` with a
            // plain literal value is fine — the value satisfies the requirement; the gate's
            // valid vectors carry exactly that shape.)
            if h.get("isSecret").and_then(Value::as_bool) == Some(true) {
                return Err(format!("header {name:?} is marked secret"));
            }
            if h.get("variables").is_some_and(|v| !v.is_null()) {
                return Err(format!("header {name:?} carries variable substitutions"));
            }
            let value = h
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("header {name:?} carries no literal value"))?;
            if value.contains('{') && value.contains('}') {
                return Err(format!("header {name:?} carries a templated value"));
            }
            headers.push((name.to_owned(), value.to_owned()));
        }
    }
    let auth = match root
        .get("_meta")
        .and_then(|m| m.get("sh.topos/auth"))
        .and_then(Value::as_str)
    {
        Some("oauth") => AuthHint::Oauth,
        Some("none") => AuthHint::None,
        _ => AuthHint::Unknown,
    };
    Ok(ParsedServer { url, headers, auth })
}

// =================================================================================================
// The converge.
// =================================================================================================

/// Converge ONE scope's MCP placements: demanded bundles land in (or update within) every engaged
/// harness's config; ledger entries whose bundle is neither demanded nor held leave — but only
/// when `allow_removals` (a targeted / frozen / blinded run never removes). `hold` carries bundle
/// ids whose demand state is UNKNOWABLE this run (an unreached workspace, a failed channel, a
/// mentioned-but-unresolved row): their entries are left byte-identical and unreported.
pub(crate) fn converge(
    io: &ScopeIo<'_>,
    demands: &[McpDemand],
    descriptors: &[McpHarness],
    detected: &BTreeSet<String>,
    hold: &HashSet<String>,
    allow_removals: bool,
) -> ConvergeOutcome {
    let mut out = ConvergeOutcome::default();

    // The ledger — fail closed whole: without it no ownership question is answerable, so nothing
    // is read or written.
    let mut ledger = match mcp_ledger::read(io.fs, io.layout) {
        Ok(l) => l,
        Err(e) => {
            out.warnings.push(format!(
                "MCP_LEDGER_UNREADABLE: {} — no MCP config is read or written this run",
                e.detail()
            ));
            return out;
        }
    };

    // Crash recovery over the intent journal, BEFORE any prior map is built (a landed-but-
    // unpromoted write would otherwise read as user drift forever).
    let dialect_of = mcp_ledger::dialect_lookup(descriptors, io.project_root.is_some());
    if ledger.recover_pending(io.fs, &dialect_of)
        && let Err(e) = mcp_ledger::write(io.fs, io.layout, &ledger)
    {
        out.warnings.push(format!(
            "MCP_LEDGER_WRITE_FAILED: {} — MCP convergence skipped this run",
            e.detail()
        ));
        return out;
    }

    // Parse every demand once. The FULL server-document gate (`mcp_validate`) re-runs on the
    // demand bytes here, fail-closed, BEFORE the placement parse: a local row's `server.json` is
    // re-read from disk every converge and can have been edited since it was adopted (a smuggled
    // credential, an http endpoint, a template), and the converge boundary trusts no earlier
    // check — content-addressed workspace bytes were gated at publish, but the re-check is one
    // call. A refusal HOLDS the bundle (its standing entries must not read as undemanded,
    // nothing new is placed) and the warning names the typed refusal code.
    let mut parsed: Vec<(usize, ParsedServer)> = Vec::new();
    let mut failed: BTreeMap<usize, String> = BTreeMap::new();
    for (i, d) in demands.iter().enumerate() {
        let gated = crate::mcp_validate::validate_server_json(&d.server_json)
            .map_err(|r| format!("{}: {}", r.code.as_str(), r.message))
            .and_then(|_| parse_server_json(&d.server_json));
        match gated {
            Ok(p) => parsed.push((i, p)),
            Err(reason) => {
                out.warnings.push(format!(
                    "MCP_UNPLACEABLE {}: {reason} — nothing is placed for it",
                    d.name
                ));
                failed.insert(i, reason);
            }
        }
    }
    let demanded_ids: HashSet<&str> = demands.iter().map(|d| d.bundle_id.as_str()).collect();
    let held = |bundle_id: &str| -> bool {
        hold.contains(bundle_id)
            || demands
                .iter()
                .enumerate()
                .any(|(i, d)| d.bundle_id == bundle_id && failed.contains_key(&i))
    };

    // Mint keys for the placeable demands (durable with the first write below).
    let keys_before = ledger.keys.clone();
    let minted: BTreeMap<usize, String> = parsed
        .iter()
        .map(|(i, _)| {
            let d = &demands[*i];
            (
                *i,
                ledger.mint_key(&d.bundle_id, &d.name, d.workspace_slug.as_deref()),
            )
        })
        .collect();
    let mut ledger_dirty = ledger.keys != keys_before;

    // Per-bundle state collector, keyed by demand index (order preserved for the receipt).
    let mut states: BTreeMap<usize, Vec<McpAgentState>> = BTreeMap::new();

    for h in descriptors {
        // The scope surface. A PROJECT scope never falls back to the user surface.
        let surface: Option<(PathBuf, McpDialect)> = match &io.project_root {
            Some(root) => match h.project_surface {
                Some((rel, dialect)) => {
                    let path = root.join(rel);
                    // THE CONTAINMENT RAIL, before ANY read or write: the resolved path (symlinks
                    // followed for the check) must stay inside the checkout — refused and
                    // disclosed, never redirected.
                    if crate::placement::within_project(root, &path) {
                        Some((path, dialect))
                    } else {
                        let line = crate::placement::escape_line(h.slug, &path);
                        if !out.warnings.contains(&line) {
                            out.warnings.push(line);
                        }
                        for (i, _) in &parsed {
                            if filter_admits(&demands[*i], h.slug) {
                                push_state(
                                    &mut states,
                                    *i,
                                    agent_state(
                                        h.slug,
                                        "unprovable",
                                        Some(
                                            "the config path does not resolve inside this checkout",
                                        ),
                                        None,
                                    ),
                                );
                            }
                        }
                        continue;
                    }
                }
                None => None,
            },
            None => h.user_surface.as_ref().and_then(|s| {
                mcp::descriptor::user_surface_path(h, &io.home).map(|p| (p, s.dialect))
            }),
        };
        let Some((path, dialect)) = surface else {
            // No surface AT THIS SCOPE: withheld, honestly, per demanded bundle.
            let note = if io.project_root.is_some() {
                "no project-level config"
            } else {
                "no user-level config"
            };
            for (i, _) in &parsed {
                if filter_admits(&demands[*i], h.slug) {
                    push_state(
                        &mut states,
                        *i,
                        agent_state(h.slug, "not-supported", Some(note), None),
                    );
                }
            }
            continue;
        };

        // Engagement: the harness is detected on this machine, OR its config surface already
        // exists (entries were placed while it was detected — removal/update must still reach
        // them).
        let engaged = detected.contains(h.slug) || io.fs.exists(&path);
        if !engaged {
            continue;
        }

        // The desired set for this harness: the placeable demands that pass the row narrowing and
        // the capability filter.
        let mut desired: Vec<McpEntry> = Vec::new();
        let mut desired_bundles: BTreeMap<String, usize> = BTreeMap::new();
        for (i, p) in &parsed {
            let d = &demands[*i];
            if !filter_admits(d, h.slug) {
                continue;
            }
            if !h.oauth_capable && p.auth != AuthHint::None {
                // The capability filter: a server that may need an OAuth sign-in is withheld from
                // an agent that cannot run one.
                push_state(
                    &mut states,
                    *i,
                    agent_state(
                        h.slug,
                        "not-supported",
                        Some("needs an OAuth sign-in this agent cannot run"),
                        None,
                    ),
                );
                continue;
            }
            let key = minted[i].clone();
            desired_bundles.insert(key.clone(), *i);
            desired.push(McpEntry {
                key,
                url: p.url.clone(),
                headers: p.headers.clone(),
                auth: p.auth,
            });
        }
        // Parse-failed demands report per engaged harness (their entries are held above).
        for (i, reason) in &failed {
            if filter_admits(&demands[*i], h.slug) {
                push_state(
                    &mut states,
                    *i,
                    agent_state(h.slug, "unprovable", Some(reason.as_str()), None),
                );
            }
        }

        // Keys PRESERVED from removal on this surface: a held bundle's, and — on a run that may
        // not remove — every undemanded bundle's. Excluded from the drivers' prior map, so the
        // drivers read them as foreign and leave them byte-identical.
        let preserved = |ledger: &McpLedger, entry_key: &str| -> bool {
            let Some(bundle) = ledger.bundle_of_key(entry_key) else {
                return false;
            };
            if demanded_ids.contains(bundle) && !held(bundle) {
                return false;
            }
            held(bundle) || !allow_removals
        };

        // Provenance for the ledger rows this surface may write: key → (bundle, version).
        let provenance: BTreeMap<String, (String, String)> = desired_bundles
            .iter()
            .map(|(key, i)| {
                let d = &demands[*i];
                (key.clone(), (d.bundle_id.clone(), d.version_id.clone()))
            })
            .collect();
        let surface_out = if dialect == McpDialect::ClaudePluginDir {
            converge_plugin_dir(io, &mut ledger, h, &path, &desired, &preserved, &provenance)
        } else {
            converge_file(
                io,
                &mut ledger,
                h,
                &path,
                dialect,
                &desired,
                &preserved,
                &provenance,
            )
        };
        ledger_dirty |= surface_out.ledger_dirty;
        out.warnings.extend(surface_out.warnings);
        for (key, state) in surface_out.states {
            match desired_bundles.get(&key) {
                Some(i) => push_state(&mut states, *i, state),
                None => {
                    // A key outside the desired set: a removal (or a drifted survivor of one),
                    // reported with its bundle.
                    if (state.state == "removed" || state.state == "drifted")
                        && let Some(bundle) = ledger.bundle_of_key(&key)
                    {
                        out.removed.push(RemovedEntry {
                            bundle_id: bundle.to_owned(),
                            state,
                        });
                    }
                }
            }
        }
    }

    // Retirement: a bundle with no remaining entries anywhere, not demanded and not held, gives
    // its key back to the reserve.
    let retire: Vec<String> = ledger
        .keys
        .keys()
        .filter(|b| !demanded_ids.contains(b.as_str()) && !held(b) && !ledger.has_entries_for(b))
        .cloned()
        .collect();
    for bundle in retire {
        if allow_removals {
            ledger.retire_key(&bundle);
            ledger_dirty = true;
        }
    }

    if ledger_dirty && let Err(e) = mcp_ledger::write(io.fs, io.layout, &ledger) {
        out.warnings
            .push(format!("MCP_LEDGER_WRITE_FAILED: {}", e.detail()));
    }

    out.bundles = demands
        .iter()
        .enumerate()
        .map(|(i, d)| BundleStates {
            bundle_id: d.bundle_id.clone(),
            name: d.name.clone(),
            states: states.remove(&i).unwrap_or_default(),
        })
        .collect();
    out
}

/// Surgically remove ONE bundle's placements from every engaged harness config in this scope —
/// the `remove` verb's inline convergence. Only the named bundle's entries move; everything else
/// in every file is left byte-identical. Drifted entries are LEFT and disclosed.
pub(crate) fn remove_bundle(
    io: &ScopeIo<'_>,
    descriptors: &[McpHarness],
    detected: &BTreeSet<String>,
    bundle_id: &str,
) -> ConvergeOutcome {
    let mut out = ConvergeOutcome::default();
    let mut ledger = match mcp_ledger::read(io.fs, io.layout) {
        Ok(l) => l,
        Err(e) => {
            out.warnings.push(format!(
                "MCP_LEDGER_UNREADABLE: {} — MCP entries are left in place",
                e.detail()
            ));
            return out;
        }
    };
    let dialect_of = mcp_ledger::dialect_lookup(descriptors, io.project_root.is_some());
    let mut ledger_dirty = ledger.recover_pending(io.fs, &dialect_of);
    let Some(key) = ledger.keys.get(bundle_id).cloned() else {
        return out; // never placed here — nothing to converge
    };

    for h in descriptors {
        let surface: Option<(PathBuf, McpDialect)> = match &io.project_root {
            Some(root) => h.project_surface.and_then(|(rel, dialect)| {
                let path = root.join(rel);
                crate::placement::within_project(root, &path).then_some((path, dialect))
            }),
            None => h.user_surface.as_ref().and_then(|s| {
                mcp::descriptor::user_surface_path(h, &io.home).map(|p| (p, s.dialect))
            }),
        };
        let Some((path, dialect)) = surface else {
            continue;
        };
        if !(detected.contains(h.slug) || io.fs.exists(&path)) {
            continue;
        }
        if !ledger.entries.contains_key(&placement_key(h.slug, &key)) {
            continue; // nothing recorded on this surface
        }
        // Prior scoped to ONLY this bundle's key: the drivers remove prior-matched undesired
        // keys, and every other entry — ours or not — reads foreign and stays byte-identical.
        let only_this = |_l: &McpLedger, entry_key: &str| entry_key != key;
        let provenance = BTreeMap::new();
        let surface_out = if dialect == McpDialect::ClaudePluginDir {
            converge_plugin_dir(io, &mut ledger, h, &path, &[], &only_this, &provenance)
        } else {
            converge_file(
                io,
                &mut ledger,
                h,
                &path,
                dialect,
                &[],
                &only_this,
                &provenance,
            )
        };
        ledger_dirty |= surface_out.ledger_dirty;
        out.warnings.extend(surface_out.warnings);
        for (state_key, state) in surface_out.states {
            if state_key == key && (state.state == "removed" || state.state == "drifted") {
                out.removed.push(RemovedEntry {
                    bundle_id: bundle_id.to_owned(),
                    state,
                });
            }
        }
    }

    if !ledger.has_entries_for(bundle_id) {
        ledger.retire_key(bundle_id);
        ledger_dirty = true;
    }
    if ledger_dirty && let Err(e) = mcp_ledger::write(io.fs, io.layout, &ledger) {
        out.warnings
            .push(format!("MCP_LEDGER_WRITE_FAILED: {}", e.detail()));
    }
    out
}

/// Whether a demand's harness narrowing admits `slug` (empty = all).
fn filter_admits(d: &McpDemand, slug: &str) -> bool {
    d.harness_filter.is_empty() || d.harness_filter.iter().any(|s| s == slug)
}

fn agent_state(slug: &str, state: &str, note: Option<&str>, file: Option<&Path>) -> McpAgentState {
    McpAgentState {
        agent: slug.to_owned(),
        state: state.to_owned(),
        note: note.map(str::to_owned),
        file: file.map(|p| p.display().to_string()),
    }
}

fn push_state(states: &mut BTreeMap<usize, Vec<McpAgentState>>, i: usize, s: McpAgentState) {
    states.entry(i).or_default().push(s);
}

// =================================================================================================
// One FILE surface (every dialect but the plugin dir): driver apply + the intent journal.
// =================================================================================================

/// What one surface's converge decided: per-KEY states (the caller joins keys back onto bundles),
/// whether the ledger moved, and the surface's warnings.
struct SurfaceOutcome {
    states: Vec<(String, McpAgentState)>,
    ledger_dirty: bool,
    warnings: Vec<String>,
}

impl SurfaceOutcome {
    fn empty() -> Self {
        Self {
            states: Vec::new(),
            ledger_dirty: false,
            warnings: Vec::new(),
        }
    }
    fn unprovable(desired: &[McpEntry], h: &McpHarness, path: &Path, reason: &str) -> Self {
        Self {
            states: desired
                .iter()
                .map(|e| {
                    (
                        e.key.clone(),
                        agent_state(h.slug, "unprovable", Some(reason), Some(path)),
                    )
                })
                .collect(),
            ledger_dirty: false,
            warnings: vec![format!("MCP_SURFACE_UNPROVABLE {}: {reason}", h.slug)],
        }
    }
}

/// The intent-journal protocol around ONE config write: (a) the ledger persists the pending
/// intents, (b) the config file is replaced, (c) the ledger promotes the intents. A crash between
/// (b) and (c) is healed by [`McpLedger::recover_pending`] next run.
fn journaled_write(
    io: &ScopeIo<'_>,
    ledger: &mut McpLedger,
    path: &Path,
    intents: BTreeMap<String, PendingIntent>,
    write: &dyn Fn() -> std::io::Result<()>,
) -> Result<(), String> {
    ledger.pending = intents;
    if let Err(e) = mcp_ledger::write(io.fs, io.layout, ledger) {
        ledger.pending.clear();
        return Err(format!("the ledger write failed ({})", e.detail()));
    }
    if let Err(e) = write() {
        // The config write did not land: drop the intents durably (best-effort — recovery would
        // reach the same answer by observing the file).
        ledger.pending.clear();
        let _ = mcp_ledger::write(io.fs, io.layout, ledger);
        return Err(format!("writing {} failed ({e})", path.display()));
    }
    // Promote: pending → entries, exactly as recovery would.
    let pending = std::mem::take(&mut ledger.pending);
    for (ledger_key, intent) in pending {
        if intent.fingerprint.is_empty() {
            ledger.entries.remove(&ledger_key);
        } else {
            ledger.entries.insert(
                ledger_key,
                LedgerEntry {
                    bundle_id: intent.bundle_id,
                    version_id: intent.version_id,
                    file: intent.file,
                    fingerprint: intent.fingerprint,
                    owns_file: intent.owns_file,
                },
            );
        }
    }
    if let Err(e) = mcp_ledger::write(io.fs, io.layout, ledger) {
        // The entries moved in memory and the intents are still journaled ON DISK — the next
        // run's recovery promotes them again. Disclose, never lose.
        return Err(format!(
            "the ledger promotion write failed ({}); recovery heals it next run",
            e.detail()
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn converge_file(
    io: &ScopeIo<'_>,
    ledger: &mut McpLedger,
    h: &McpHarness,
    path: &Path,
    dialect: McpDialect,
    desired: &[McpEntry],
    preserved: &dyn Fn(&McpLedger, &str) -> bool,
    provenance: &BTreeMap<String, (String, String)>,
) -> SurfaceOutcome {
    let mut prior = ledger.prior_for(h.slug);
    let kept: BTreeSet<String> = prior
        .keys()
        .filter(|k| preserved(ledger, k))
        .cloned()
        .collect();
    for k in &kept {
        prior.remove(k);
    }
    let current = match io.fs.read_opt(path) {
        Ok(c) => c,
        Err(e) => {
            return SurfaceOutcome::unprovable(
                desired,
                h,
                path,
                &format!("reading {} failed ({e})", path.display()),
            );
        }
    };
    // Nothing desired, nothing ours on record: leave the surface untouched (never parse-probe a
    // foreign file for no reason).
    if desired.is_empty() && prior.is_empty() {
        return SurfaceOutcome::empty();
    }
    // Whether the file holds ANY content beyond topos-managed entries. The drivers classify only
    // managed-LOOKING keys, so a plain user entry added to a topos-created file is invisible to
    // their states — this is the sighting that makes the whole-file-ownership flag stop lying.
    let unmanaged = mcp::holds_unmanaged_content(dialect, current.as_deref());
    let outcome = mcp::apply(dialect, current.as_deref(), desired, &prior);
    match outcome.plan {
        EditPlan::Unprovable(reason) => SurfaceOutcome::unprovable(desired, h, path, &reason),
        EditPlan::Leave => {
            let mut surface = fold_states(h, path, &outcome.states, desired);
            surface.ledger_dirty = sync_ledger_entries(
                ledger,
                h.slug,
                path,
                &outcome.fingerprints,
                &kept,
                provenance,
                false,
            );
            if unmanaged {
                surface.ledger_dirty |= clear_owns_file(ledger, h.slug, path);
            }
            surface
        }
        EditPlan::Write(bytes) => {
            // Whole-file ownership for the NEXT ledger state: topos creates the file now, or it
            // owned every byte at the last write AND this reconcile saw nothing that is not ours
            // — neither a drifted/foreign managed entry NOR any unmanaged content.
            let owned_before = {
                let prefix = format!("{}/", h.slug);
                let mine: Vec<&LedgerEntry> = ledger
                    .entries
                    .iter()
                    .filter(|(k, e)| k.starts_with(&prefix) && Path::new(&e.file) == path)
                    .map(|(_, e)| e)
                    .collect();
                !mine.is_empty() && mine.iter().all(|e| e.owns_file)
            };
            let all_ours = outcome
                .states
                .iter()
                .all(|(_, s)| !matches!(s, EntryState::Drifted | EntryState::Foreign));
            let owns_file =
                outcome.created_file || (owned_before && all_ours && kept.is_empty() && !unmanaged);

            let intents = write_intents(
                ledger,
                h.slug,
                path,
                &outcome.fingerprints,
                &kept,
                provenance,
                owns_file,
            );
            let path_owned = path.to_path_buf();
            let fs = io.fs;
            let write = move || crate::config_io::replace_config(fs, &path_owned, &bytes);
            match journaled_write(io, ledger, path, intents, &write) {
                Ok(()) => {
                    let mut surface = fold_states(h, path, &outcome.states, desired);
                    surface.ledger_dirty = true;
                    // The file topos wholly owned just lost its LAST entry: nothing of ours
                    // remains — delete it (through the seam; the write above already proved the
                    // path safe). CONSERVATIVE BELT before any whole-file delete: RE-READ the
                    // just-written post-image and delete ONLY when it is structurally empty —
                    // no managed entries left AND nothing beyond the fresh-creation skeleton.
                    // The ownership flag is a recorded claim; the post-image is the fact.
                    if desired.is_empty()
                        && owns_file
                        && !ledger.entries.iter().any(|(k, e)| {
                            k.starts_with(&format!("{}/", h.slug)) && Path::new(&e.file) == path
                        })
                        && post_image_structurally_empty(io, dialect, path)
                        && io.fs.remove_file(path).is_ok()
                    {
                        surface.warnings.push(format!(
                            "MCP_FILE_REMOVED {}: {} held only topos entries and was deleted",
                            h.slug,
                            path.display()
                        ));
                    }
                    surface
                }
                Err(reason) => SurfaceOutcome::unprovable(desired, h, path, &reason),
            }
        }
    }
}

/// Belt two of the whole-file-ownership discipline: the ledger's `owns_file` flags for `path` go
/// false the moment a converge OBSERVES content in the file beyond topos's managed entries — so
/// a later removal can never trust a flag the file has outgrown.
fn clear_owns_file(ledger: &mut McpLedger, slug: &str, path: &Path) -> bool {
    let prefix = format!("{slug}/");
    let mut dirty = false;
    for (k, e) in &mut ledger.entries {
        if k.starts_with(&prefix) && Path::new(&e.file) == path && e.owns_file {
            e.owns_file = false;
            dirty = true;
        }
    }
    dirty
}

/// Belt one, at the delete boundary: re-read the just-written post-image and answer whether
/// NOTHING but our removal remains — provably readable, zero managed entries, and no content
/// beyond what a fresh topos-created file would hold. Any read failure or indeterminate shape
/// answers `false` (the file is kept; a stray skeleton is recoverable, destroyed bytes are not).
fn post_image_structurally_empty(io: &ScopeIo<'_>, dialect: McpDialect, path: &Path) -> bool {
    let Ok(post) = io.fs.read_opt(path) else {
        return false;
    };
    let observed = mcp::observe(dialect, post.as_deref());
    observed.parseable
        && observed.entries.is_empty()
        && !mcp::holds_unmanaged_content(dialect, post.as_deref())
}

/// The pending intents one Write commits: every fingerprint that differs from the standing entry
/// (add/update), plus a removal intent for every non-preserved standing key the outcome no longer
/// carries.
#[allow(clippy::too_many_arguments)]
fn write_intents(
    ledger: &McpLedger,
    slug: &str,
    path: &Path,
    fingerprints: &[(String, String)],
    kept: &BTreeSet<String>,
    provenance: &BTreeMap<String, (String, String)>,
    owns_file: bool,
) -> BTreeMap<String, PendingIntent> {
    let file = path.display().to_string();
    let mut intents = BTreeMap::new();
    let next: BTreeMap<&str, &str> = fingerprints
        .iter()
        .map(|(k, f)| (k.as_str(), f.as_str()))
        .collect();
    let bundle_of = |key: &str| -> (String, String) {
        // The demanded bundle's provenance wins (it carries the fresh version); a drifted
        // survivor keeps its standing row's.
        provenance.get(key).cloned().unwrap_or_else(|| {
            ledger
                .bundle_of_key(key)
                .map(|b| {
                    let version = ledger
                        .entries
                        .get(&placement_key(slug, key))
                        .map(|e| e.version_id.clone())
                        .unwrap_or_default();
                    (b.to_owned(), version)
                })
                .unwrap_or_default()
        })
    };
    for (key, fp) in &next {
        let ledger_key = placement_key(slug, key);
        let standing = ledger.entries.get(&ledger_key);
        let changed = standing.map(|e| (e.fingerprint.as_str(), e.owns_file, e.file.as_str()))
            != Some((*fp, owns_file, file.as_str()));
        if changed {
            let (bundle_id, version_id) = bundle_of(key);
            intents.insert(
                ledger_key,
                PendingIntent {
                    bundle_id,
                    version_id,
                    file: file.clone(),
                    fingerprint: (*fp).to_owned(),
                    owns_file,
                },
            );
        }
    }
    let prefix = format!("{slug}/");
    for (ledger_key, entry) in &ledger.entries {
        let Some(key) = ledger_key.strip_prefix(&prefix) else {
            continue;
        };
        if kept.contains(key) || next.contains_key(key) {
            continue;
        }
        intents.insert(
            ledger_key.clone(),
            PendingIntent {
                bundle_id: entry.bundle_id.clone(),
                version_id: String::new(),
                file: file.clone(),
                fingerprint: String::new(),
                owns_file: false,
            },
        );
    }
    intents
}

/// Sync ledger entries to a LEAVE outcome's fingerprints (no config write happened; the ledger
/// just tracks what the file provably holds — e.g. a prior key that vanished from the file).
#[allow(clippy::too_many_arguments)]
fn sync_ledger_entries(
    ledger: &mut McpLedger,
    slug: &str,
    path: &Path,
    fingerprints: &[(String, String)],
    kept: &BTreeSet<String>,
    provenance: &BTreeMap<String, (String, String)>,
    owns_file_default: bool,
) -> bool {
    let file = path.display().to_string();
    let next: BTreeMap<&str, &str> = fingerprints
        .iter()
        .map(|(k, f)| (k.as_str(), f.as_str()))
        .collect();
    let mut dirty = false;
    // Drop standing entries the surface no longer carries (not the preserved ones).
    let prefix = format!("{slug}/");
    let stale: Vec<String> = ledger
        .entries
        .keys()
        .filter(|k| {
            k.strip_prefix(&prefix)
                .is_some_and(|key| !next.contains_key(key) && !kept.contains(key))
        })
        .cloned()
        .collect();
    for k in stale {
        ledger.entries.remove(&k);
        dirty = true;
    }
    for (key, fp) in &next {
        let ledger_key = placement_key(slug, key);
        let (bundle, version) = provenance.get(*key).cloned().unwrap_or_else(|| {
            (
                ledger.bundle_of_key(key).unwrap_or_default().to_owned(),
                String::new(),
            )
        });
        let entry = ledger.entries.entry(ledger_key).or_insert_with(|| {
            dirty = true;
            LedgerEntry {
                bundle_id: bundle.clone(),
                version_id: version.clone(),
                file: file.clone(),
                fingerprint: String::new(),
                owns_file: owns_file_default,
            }
        });
        if entry.fingerprint != **fp || entry.file != file {
            entry.fingerprint = (**fp).to_owned();
            entry.file = file.clone();
            dirty = true;
        }
    }
    dirty
}

/// Fold driver per-key states into per-key wire states (see the module doc's vocabulary).
fn fold_states(
    h: &McpHarness,
    path: &Path,
    states: &[(String, EntryState)],
    desired: &[McpEntry],
) -> SurfaceOutcome {
    let desired_keys: BTreeSet<&str> = desired.iter().map(|e| e.key.as_str()).collect();
    let mut out = SurfaceOutcome::empty();
    for (key, st) in states {
        let mapped = match st {
            // A NEW or CHANGED placement carries the harness's reload note — how the change goes
            // live.
            EntryState::PlacedNew | EntryState::Updated => {
                agent_state(h.slug, "current", Some(h.reload_note), Some(path))
            }
            EntryState::Current => agent_state(h.slug, "current", None, Some(path)),
            EntryState::Drifted => agent_state(
                h.slug,
                "drifted",
                Some("hand-edited since topos wrote it — left in place"),
                Some(path),
            ),
            EntryState::Foreign if desired_keys.contains(key.as_str()) => agent_state(
                h.slug,
                "conflicting",
                Some("the config key is held by an entry topos does not own"),
                Some(path),
            ),
            EntryState::Foreign => continue,
            EntryState::Removed => agent_state(h.slug, "removed", None, Some(path)),
        };
        out.states.push((key.clone(), mapped));
    }
    out
}

// =================================================================================================
// The Claude Code plugin dir — a wholly-topos-owned DIRECTORY, rendered whole.
// =================================================================================================

#[allow(clippy::too_many_arguments)]
fn converge_plugin_dir(
    io: &ScopeIo<'_>,
    ledger: &mut McpLedger,
    h: &McpHarness,
    dir: &Path,
    desired: &[McpEntry],
    preserved: &dyn Fn(&McpLedger, &str) -> bool,
    provenance: &BTreeMap<String, (String, String)>,
) -> SurfaceOutcome {
    let mcp_path = dir.join(plugin_dir::PLUGIN_MCP_PATH);
    let manifest_path = dir.join(plugin_dir::PLUGIN_MANIFEST_PATH);
    let prior = ledger.prior_for(h.slug);
    let kept: BTreeSet<String> = prior
        .keys()
        .filter(|k| preserved(ledger, k))
        .cloned()
        .collect();

    let current = match io.fs.read_opt(&mcp_path) {
        Ok(c) => c,
        Err(e) => {
            return SurfaceOutcome::unprovable(
                desired,
                h,
                &mcp_path,
                &format!("reading {} failed ({e})", mcp_path.display()),
            );
        }
    };
    if desired.is_empty() && prior.is_empty() && current.is_none() {
        return SurfaceOutcome::empty();
    }
    // Parse the current file OURSELVES (strict JSON): the dir is wholly topos-owned, so the
    // values are needed verbatim to carry held entries through a whole-dir rewrite.
    let found_values: BTreeMap<String, Value> = match &current {
        None => BTreeMap::new(),
        Some(bytes) => match serde_json::from_slice::<Value>(bytes)
            .ok()
            .as_ref()
            .and_then(|v| v.get("mcpServers"))
            .and_then(Value::as_object)
        {
            Some(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            None => {
                // A foreign occupant (unparseable / wrong shape) of the topos-owned dir: back
                // off whole.
                return SurfaceOutcome::unprovable(
                    desired,
                    h,
                    &mcp_path,
                    "the topos plugin dir holds content topos did not write",
                );
            }
        },
    };
    let found_fps: BTreeMap<String, String> = found_values
        .iter()
        .map(|(k, v)| (k.clone(), mcp::fingerprint_value(v)))
        .collect();
    let desired_values: BTreeMap<String, Value> = desired
        .iter()
        .map(|e| {
            (
                e.key.clone(),
                mcp::entry_value(McpDialect::ClaudePluginDir, e),
            )
        })
        .collect();
    let desired_fps: BTreeMap<String, String> = desired_values
        .iter()
        .map(|(k, v)| (k.clone(), mcp::fingerprint_value(v)))
        .collect();

    // Classify every present entry. The dir is wholly ours, so anything neither at its desired
    // value nor prior-matched is drift (ledger-known) or foreign (not ours at all).
    let mut drifted: BTreeSet<String> = BTreeSet::new();
    for (k, fp) in &found_fps {
        if desired_fps.get(k) == Some(fp) || prior.get(k) == Some(fp) {
            continue;
        }
        if prior.contains_key(k) || kept.contains(k) {
            drifted.insert(k.clone());
        } else {
            return SurfaceOutcome::unprovable(
                desired,
                h,
                &mcp_path,
                "the topos plugin dir holds an entry topos did not write",
            );
        }
    }
    if !drifted.is_empty() {
        // A hand-edited entry in a whole-rendered dir blocks the rewrite (never overwritten).
        let mut out = SurfaceOutcome::empty();
        for e in desired {
            let state = if drifted.contains(&e.key) {
                agent_state(
                    h.slug,
                    "drifted",
                    Some("hand-edited since topos wrote it — left in place"),
                    Some(&mcp_path),
                )
            } else if found_fps.get(&e.key) == desired_fps.get(&e.key) {
                agent_state(h.slug, "current", None, Some(&mcp_path))
            } else {
                agent_state(
                    h.slug,
                    "unprovable",
                    Some("held back: a hand-edited entry blocks rewriting the topos plugin dir"),
                    Some(&mcp_path),
                )
            };
            out.states.push((e.key.clone(), state));
        }
        for k in &drifted {
            if !desired_fps.contains_key(k) {
                out.states.push((
                    k.clone(),
                    agent_state(
                        h.slug,
                        "drifted",
                        Some("hand-edited since topos wrote it — left in place"),
                        Some(&mcp_path),
                    ),
                ));
            }
        }
        return out;
    }

    // The NEXT file content: the desired entries, plus every held entry's current value verbatim.
    let mut next_values = desired_values.clone();
    for k in &kept {
        if let Some(v) = found_values.get(k) {
            next_values.insert(k.clone(), v.clone());
        }
    }

    if next_values.is_empty() {
        // Removal of the last entries. Ownership proof: every present entry was classified ours
        // above (prior-matched — drift/foreign already returned), so the two files go, and the
        // dirs are pruned when they hold nothing else.
        if found_values.is_empty() && current.is_none() {
            return SurfaceOutcome::empty();
        }
        let intents: BTreeMap<String, PendingIntent> = found_fps
            .keys()
            .map(|k| {
                (
                    placement_key(h.slug, k),
                    PendingIntent {
                        bundle_id: ledger.bundle_of_key(k).unwrap_or_default().to_owned(),
                        version_id: String::new(),
                        file: mcp_path.display().to_string(),
                        fingerprint: String::new(),
                        owns_file: false,
                    },
                )
            })
            .collect();
        let fs = io.fs;
        let mcp_p = mcp_path.clone();
        let manifest_p = manifest_path.clone();
        let dir_p = dir.to_path_buf();
        let write = move || -> std::io::Result<()> {
            fs.remove_file(&mcp_p)?;
            let _ = fs.remove_file(&manifest_p);
            // Prune the dirs only when they hold nothing else (the ownership proof extended to
            // the directory level).
            let only = |d: &Path, expect: &[&str]| -> bool {
                fs.read_dir(d).is_ok_and(|entries| {
                    entries.iter().all(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| expect.contains(&n))
                    })
                })
            };
            let claude_plugin = dir_p.join(".claude-plugin");
            if fs.exists(&claude_plugin) && only(&claude_plugin, &[]) {
                let _ = fs.remove_dir_all(&claude_plugin);
            }
            if only(&dir_p, &[]) {
                let _ = fs.remove_dir_all(&dir_p);
            }
            Ok(())
        };
        return match journaled_write(io, ledger, &mcp_path, intents, &write) {
            Ok(()) => {
                let mut out = SurfaceOutcome::empty();
                out.ledger_dirty = true;
                for k in found_fps.keys() {
                    out.states.push((
                        k.clone(),
                        agent_state(h.slug, "removed", None, Some(&mcp_path)),
                    ));
                }
                out
            }
            Err(reason) => SurfaceOutcome::unprovable(desired, h, &mcp_path, &reason),
        };
    }

    let next_fps: BTreeMap<String, String> = next_values
        .iter()
        .map(|(k, v)| (k.clone(), mcp::fingerprint_value(v)))
        .collect();
    let unchanged = next_fps == found_fps && io.fs.exists(&manifest_path);
    // Per-key outcome states (desired keys + removals), computed against the PRE-write file.
    let mut states: Vec<(String, McpAgentState)> = Vec::new();
    for e in desired {
        let state = match found_fps.get(&e.key) {
            None => agent_state(h.slug, "current", Some(h.reload_note), Some(&mcp_path)),
            Some(fp) if desired_fps.get(&e.key) == Some(fp) => {
                agent_state(h.slug, "current", None, Some(&mcp_path))
            }
            Some(_) => agent_state(h.slug, "current", Some(h.reload_note), Some(&mcp_path)),
        };
        states.push((e.key.clone(), state));
    }
    for k in found_fps.keys() {
        if !next_values.contains_key(k) {
            states.push((
                k.clone(),
                agent_state(h.slug, "removed", None, Some(&mcp_path)),
            ));
        }
    }

    if unchanged {
        let fps: Vec<(String, String)> = next_fps.into_iter().collect();
        let dirty = sync_ledger_entries(ledger, h.slug, &mcp_path, &fps, &kept, provenance, true);
        return SurfaceOutcome {
            states,
            ledger_dirty: dirty,
            warnings: Vec::new(),
        };
    }

    // Render whole: the constant manifest + the entries file (keys sorted, 2-space, trailing
    // newline — the plugin_dir shape).
    let manifest_bytes = plugin_dir::render_plugin_dir(&[])
        .into_iter()
        .find(|(p, _)| p == plugin_dir::PLUGIN_MANIFEST_PATH)
        .map(|(_, b)| b)
        .unwrap_or_default();
    let mut root = serde_json::Map::new();
    root.insert(
        "mcpServers".to_owned(),
        Value::Object(
            next_values
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
    );
    let mut mcp_bytes =
        serde_json::to_string_pretty(&Value::Object(root)).unwrap_or_else(|_| "{}".into());
    mcp_bytes.push('\n');

    let owns_file = true; // the dir is wholly topos-owned by construction
    let mut intents = BTreeMap::new();
    for (k, fp) in &next_fps {
        let ledger_key = placement_key(h.slug, k);
        let standing = ledger.entries.get(&ledger_key);
        if standing.map(|e| e.fingerprint.as_str()) != Some(fp.as_str()) {
            let (bundle_id, version_id) = provenance.get(k).cloned().unwrap_or_else(|| {
                (
                    ledger.bundle_of_key(k).unwrap_or_default().to_owned(),
                    String::new(),
                )
            });
            intents.insert(
                ledger_key,
                PendingIntent {
                    bundle_id,
                    version_id,
                    file: mcp_path.display().to_string(),
                    fingerprint: fp.clone(),
                    owns_file,
                },
            );
        }
    }
    let prefix = format!("{}/", h.slug);
    for ledger_key in ledger.entries.keys() {
        if let Some(key) = ledger_key.strip_prefix(&prefix)
            && !next_fps.contains_key(key)
        {
            intents.insert(
                ledger_key.clone(),
                PendingIntent {
                    bundle_id: ledger
                        .entries
                        .get(ledger_key)
                        .map(|e| e.bundle_id.clone())
                        .unwrap_or_default(),
                    version_id: String::new(),
                    file: mcp_path.display().to_string(),
                    fingerprint: String::new(),
                    owns_file: false,
                },
            );
        }
    }
    let fs = io.fs;
    let manifest_p = manifest_path.clone();
    let mcp_p = mcp_path.clone();
    let write = move || -> std::io::Result<()> {
        crate::config_io::replace_config(fs, &manifest_p, &manifest_bytes)?;
        crate::config_io::replace_config(fs, &mcp_p, mcp_bytes.as_bytes())
    };
    match journaled_write(io, ledger, &mcp_path, intents, &write) {
        Ok(()) => SurfaceOutcome {
            states,
            ledger_dirty: true,
            warnings: Vec::new(),
        },
        Err(reason) => SurfaceOutcome::unprovable(desired, h, &mcp_path, &reason),
    }
}

/// Whether this scope's store tracks `skill_id` as a CONFIG-PLACED (mcp) bundle — the scope's
/// ledger minted (or retired) a config key for it. ONE rung of [`record_kind`]'s chain (the
/// ledger is deletable state, so it is consulted LAST); also the publish preflight's cheapest
/// source. Best-effort: an unreadable ledger answers `false` (the converge already warned loudly
/// about it).
pub(crate) fn is_mcp_record(ctx: &crate::ctx::Ctx<'_>, skill_id: &str) -> bool {
    mcp_ledger::read(ctx.fs, &ctx.layout)
        .map(|l| l.keys.contains_key(skill_id) || l.retired.values().any(|b| b == skill_id))
        .unwrap_or(false)
}

// =================================================================================================
// The durable kind marker + the classification the per-skill engine paths trust.
// =================================================================================================

/// The tiny per-skill `kind.json` marker (`skills/<id>/kind.json`, beside `map.json`): the ONE
/// durable record of what a store-only bundle IS. Written at the first mcp sync/adopt in each
/// scope store, never rewritten, never deleted by any sweep — so a lost config ledger cannot make
/// an mcp record classify as a skill and get `server.json` materialized into skill dirs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct KindDoc {
    pub schema_version: u32,
    pub kind: String,
}

/// The marker's word for `sid` in `layout`'s store, when one is recorded. Best-effort read: an
/// unreadable marker answers `None` (classification falls through its other rungs, and the
/// empty-map arm fails CLOSED).
pub(crate) fn kind_marker(
    fs: &dyn FsOps,
    layout: &Layout,
    sid: &crate::id::SkillId,
) -> Option<String> {
    crate::doc::read_doc::<KindDoc>(fs, &layout.published(sid).kind)
        .ok()
        .flatten()
        .map(|d| d.kind)
}

/// Record `sid` as an mcp bundle in this scope's store — idempotent, best-effort (the marker is a
/// belt: classification fails closed without it, so a failed write degrades to a refusal, never to
/// a wrong placement).
pub(crate) fn write_kind_marker(ctx: &crate::ctx::Ctx<'_>, sid: &crate::id::SkillId) {
    let path = ctx.layout.published(sid).kind;
    if matches!(crate::doc::read_doc::<KindDoc>(ctx.fs, &path), Ok(Some(_))) {
        return;
    }
    let _ = crate::doc::write_doc(
        ctx.fs,
        &path,
        &KindDoc {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            kind: "mcp".to_owned(),
        },
    );
}

/// What a tracked record IS, for the paths that must decide between skill-dir placement and the
/// config converge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordKind {
    /// A config-placed (mcp) bundle — skill-dir placement must never engage.
    Mcp,
    /// An ordinary skill — the dir-placement planner runs.
    Skill,
    /// No source answered. Over an EMPTY placement map the caller must FAIL CLOSED (refuse skill
    /// planning with a typed message) — a store-only mcp record whose evidence was lost must never
    /// have `server.json` materialized into skill dirs.
    Indeterminate,
}

/// Classify `skill_id` in THIS scope: the durable marker → the offline delivery cache → the
/// manifest row that demands the placements → the config ledger. Each rung answers only when it
/// genuinely knows; a cache entry with no `kind` tag IS an answer (the cache spells `kind` only
/// when it is not the default `"skill"`).
pub(crate) fn record_kind(
    ctx: &crate::ctx::Ctx<'_>,
    skill_id: &str,
    map: &topos_types::persisted::PlacementMap,
) -> RecordKind {
    if let Ok(sid) = crate::id::SkillId::parse(skill_id)
        && let Some(kind) = kind_marker(ctx.fs, &ctx.layout, &sid)
    {
        return if kind == "mcp" {
            RecordKind::Mcp
        } else {
            RecordKind::Skill
        };
    }
    if let Ok(cache) = crate::sync_status::read(ctx.fs, &ctx.layout)
        && let Some(delivered) = cache
            .workspaces
            .values()
            .find_map(|ws| ws.delivered.get(skill_id))
    {
        return if delivered.kind.as_deref() == Some("mcp") {
            RecordKind::Mcp
        } else {
            RecordKind::Skill
        };
    }
    if !map.placements.is_empty() {
        let dirs: Vec<PathBuf> = map.placements.iter().map(PathBuf::from).collect();
        if let Ok(Some(kind)) = crate::ops::path_row_kind(ctx, &dirs) {
            return if kind == "mcp" {
                RecordKind::Mcp
            } else {
                RecordKind::Skill
            };
        }
    }
    if is_mcp_record(ctx, skill_id) {
        return RecordKind::Mcp;
    }
    RecordKind::Indeterminate
}

/// Converge THIS scope's config entries for ONE bundle right now — the store/lock just moved (a
/// go-back), and the command must not return success while agent configs still carry the previous
/// document. Same wiring as the sweep's converge, narrowed to one demand; removals stay OFF (a
/// targeted verb never touches another bundle's entries). Best-effort by construction: the store
/// move already landed, and the next sweep reaches the same configs — failures come back as
/// warning lines beside the per-agent states.
pub(crate) fn converge_bundle_now(
    ctx: &crate::ctx::Ctx<'_>,
    sid: &crate::id::SkillId,
    name: &str,
) -> (Vec<McpAgentState>, Vec<String>) {
    let Some(roots) = ctx.roots.clone() else {
        return (Vec::new(), Vec::new());
    };
    let Ok(Some((version_id, server_json))) = stored_server_json(ctx, sid) else {
        return (Vec::new(), Vec::new());
    };
    let project_root = ctx.layout.project_root().map(Path::to_path_buf);
    let cwd = project_root.clone().or_else(|| roots.cwd.clone());
    let detected: BTreeSet<String> =
        topos_harness::registry::detected_harnesses(&roots.home, cwd.as_deref())
            .iter()
            .map(|h| h.slug.to_owned())
            .collect();
    let io = ScopeIo {
        fs: ctx.fs,
        layout: &ctx.layout,
        home: roots.home.clone(),
        project_root,
    };
    // The row's harness narrowing is not re-derived here (it lives in the scope plan the sweep
    // resolves); the minted key already exists for a placed bundle, and the next sweep re-applies
    // any narrowing. `workspace_slug: None` is safe for the same reason — the key is reused, not
    // re-minted.
    let demand = McpDemand {
        bundle_id: sid.as_str().to_owned(),
        name: name.to_owned(),
        workspace_slug: None,
        version_id,
        server_json,
        harness_filter: Vec::new(),
    };
    let outcome = converge(
        &io,
        std::slice::from_ref(&demand),
        mcp::descriptor::mcp_harnesses(),
        &detected,
        &HashSet::new(),
        false,
    );
    let states = outcome
        .bundles
        .into_iter()
        .find(|b| b.bundle_id == demand.bundle_id)
        .map(|b| b.states)
        .unwrap_or_default();
    (states, outcome.warnings)
}

// =================================================================================================
// The store-read helper the reconcile's demand sites share.
// =================================================================================================

/// Read a stored MCP bundle's `server.json` from its scope store's CURRENT tree (the lock's
/// pinned version, rendered verified). `Ok(None)` when the store holds no received version yet.
pub(crate) fn stored_server_json(
    ctx: &crate::ctx::Ctx<'_>,
    sid: &crate::id::SkillId,
) -> Result<Option<(String, Vec<u8>)>, ClientError> {
    let sp = ctx.layout.published(sid);
    let Some(lock) = crate::doc::read_doc::<topos_types::persisted::Lock>(ctx.fs, &sp.lock)? else {
        return Ok(None);
    };
    if lock.base_commit.is_empty() || lock.base_commit.bytes().all(|b| b == b'0') {
        return Ok(None);
    }
    let commit = crate::ops::parse_hex32(&lock.base_commit)?;
    let digest = crate::ops::parse_hex32(&lock.bundle_digest)?;
    let store = topos_gitstore::Store::open(&sp.store)?;
    let bundle = store.render_verified(commit, digest)?;
    let server = bundle
        .files
        .iter()
        .find(|f| f.path == "server.json")
        .map(|f| f.bytes.clone());
    match server {
        Some(bytes) => Ok(Some((lock.base_commit, bytes))),
        None => Err(ClientError::Corrupt(format!(
            "{}: an mcp bundle without a server.json at its root",
            lock.name
        ))),
    }
}
