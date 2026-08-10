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
//! - the [`crate::config_custody`] discipline: key minting, the `key → fingerprint` prior map
//!   (read off each bundle's OWN record — the same `map.json` that records a skill bundle's dirs),
//!   the intent journal around EVERY config write, and crash recovery at converge start,
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
//! - the wholly-topos-owned Claude Code plugin dir: its `.mcp.json` rides the SAME shared file
//!   converge as every dialect; any content topos did not write backs the surface off whole
//!   (unprovable, byte-identical), the constant manifest is written/healed beside it only while
//!   its bytes are provably topos's own (absent, or byte-identical to the rendering — a
//!   hand-edited manifest is left standing, disclosed), and the dir is pruned when the last
//!   entry leaves.
//!
//! The engine is OFFLINE by construction: demands carry the stored `server.json` bytes, so a dead
//! network still heals config files from the store + the custody records.
//!
//! Wire states (an OPEN vocabulary, kept small): `placed` (THIS run wrote the entry — a first
//! placement, an update to it, or the repair of one that was gone) · `current` (found already in
//! order; nothing written) · `drifted` (hand-edited since topos wrote it — untouched) ·
//! `not-supported` (withheld: capability or no surface at this scope) · `unprovable` (the surface
//! cannot be safely edited) · `conflicting` (the desired config key is occupied by an entry topos
//! does not own) · `removed` (removal receipts only).
//!
//! `placed` vs `current` is the ONE fact both channels answer with: the JSON state and the words a
//! person reads come from the same [`EntryState`], so a receipt can never say a file was written
//! while the wire calls it merely current.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;
use topos_harness::mcp::{self, AuthHint, EditPlan, EntryState, McpDialect, McpEntry, plugin_dir};
use topos_harness::registry::KnownHarness;
use topos_types::results::McpAgentState;

use crate::config_custody::{self, PendingIntent, ScopeEntries, placement_key};
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;
use topos_types::persisted::EntryPlacement;

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
    /// Registry slugs this demand is narrowed to — the harnesses whose config files the row's
    /// `dest` names. `None` = no dest (every MCP-capable harness); `Some` = exactly these,
    /// possibly none at all (a dest row is frozen to what it names).
    pub harness_filter: Option<Vec<String>>,
}

/// The scope's I/O: the fs seam, the scope store (where the custody document and every bundle's own
/// record live), and the machine roots the surfaces resolve against.
pub(crate) struct ScopeIo<'a> {
    pub fs: &'a dyn FsOps,
    /// The scope's store layout — where `state/config_custody.json` and the per-bundle records live.
    pub layout: &'a Layout,
    /// The machine home (user surfaces resolve under it).
    pub home: PathBuf,
    /// `Some` = the PROJECT scope: surfaces are the project-relative ones, containment-proven.
    pub project_root: Option<PathBuf>,
}

/// One bundle's per-agent outcome this converge — answered by the IDENTITY the demand was filed
/// under (receipt rows join on it; the display name is the row's own business).
#[derive(Debug)]
pub(crate) struct BundleStates {
    pub bundle_id: String,
    pub states: Vec<McpAgentState>,
    /// Whether this converge WROTE this bundle's entry into some agent's config — a first
    /// placement, an update to it, or the repair of one a person deleted by hand. It is the
    /// only honest answer to "did the disk move for this bundle": a store that is already
    /// current says nothing about the config files, and the receipt's own verb depends on the
    /// difference. The custody records are topos's own bookkeeping, so a run that only rewrote
    /// THOSE wrote nothing here. The `placed` states in `states` name the FILES those writes
    /// landed in.
    pub wrote: bool,
    /// Whether those writes are this scope's FIRST config entries for the bundle — the durable
    /// signal being the bundle's own record, which held no entry for it when this converge began. A first
    /// placement is an INSTALL however the store got its bytes; a write over a bundle the ledger
    /// already placed is a repair. False whenever `wrote` is.
    pub first_placement: bool,
}

/// One removed placement (removal convergence / the `remove` verb's inline converge).
#[derive(Debug)]
pub(crate) struct RemovedEntry {
    /// The bundle whose entry left — the `remove` verb matches its receipts by it, and the
    /// sweep's `removed` receipt rows group their config files under it.
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
            //
            // DISCIPLINE: this block is a MATCHING RE-CHECK of the shared validation gate
            // (`crate::mcp_validate`), rule for rule. Every refusal the engine could make
            // BEYOND the gate either moves into the gate (with shared vectors, both tiers) or
            // is deleted — the engine never grows a private rule, because a bundle the gate
            // publishes must never be permanently unplaceable here.
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
    descriptors: &[&'static KnownHarness],
    detected: &BTreeSet<String>,
    hold: &HashSet<String>,
    allow_removals: bool,
) -> ConvergeOutcome {
    let mut out = ConvergeOutcome::default();
    let _lock = match converge_lock(io) {
        Ok(guard) => guard,
        Err(warning) => {
            out.warnings.push(warning);
            return out;
        }
    };

    // The scope's custody picture — fail closed whole: without it no ownership question is
    // answerable, so nothing is read or written.
    let mut ledger = match ScopeEntries::load(io.fs, io.layout) {
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
    let dialect_of = config_custody::dialect_lookup(descriptors, io.project_root.is_some());
    if ledger.recover(io.fs, &dialect_of) {
        let failures = ledger.flush(io.fs, io.layout);
        if !failures.is_empty() {
            out.warnings.extend(failures);
            out.warnings.push(
                "MCP_LEDGER_WRITE_FAILED: recovery could not be recorded — MCP convergence \
                 skipped this run"
                    .to_owned(),
            );
            return out;
        }
    }

    // The bundles this scope had ALREADY placed when the converge began — read off the durable
    // records after recovery, so it is the record and not a guess. A bundle absent here whose
    // entry this run writes is being placed for the FIRST time in this scope (see
    // [`BundleStates::first_placement`]).
    let placed_before: HashSet<String> = ledger.placed_bundles().into_iter().collect();

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

    // Per-bundle state collector, keyed by demand index (order preserved for the receipt).
    let mut states: BTreeMap<usize, Vec<McpAgentState>> = BTreeMap::new();
    // The demands whose config entries this run WROTE somewhere — see [`BundleStates::wrote`].
    let mut wrote: BTreeSet<usize> = BTreeSet::new();

    for h in descriptors {
        // The scope surface. A PROJECT scope never falls back to the user surface.
        let surface: Option<(PathBuf, McpDialect)> = match &io.project_root {
            Some(root) => match h.mcp().and_then(|m| m.project) {
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
            None => h
                .mcp()
                .and_then(|m| m.user)
                .and_then(|s| h.mcp_user_path(&io.home).map(|p| (p, s.dialect))),
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

        // The desired set for this harness: the placeable demands that pass the row narrowing.
        let mut desired: Vec<McpEntry> = Vec::new();
        let mut desired_bundles: BTreeMap<String, usize> = BTreeMap::new();
        for (i, p) in &parsed {
            let d = &demands[*i];
            if !filter_admits(d, h.slug) {
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
        let preserved = |ledger: &ScopeEntries, entry_key: &str| -> bool {
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
        // The plugin dir's driver surface is its `.mcp.json`; the manifest beside it and the
        // dir prune are `converge_file`'s dialect-specific I/O.
        let path = surface_file(&path, dialect);
        let surface_out = converge_file(
            io,
            &mut ledger,
            h,
            &path,
            dialect,
            &desired,
            &preserved,
            &provenance,
        );
        out.warnings.extend(surface_out.warnings);
        for key in &surface_out.wrote {
            if let Some(i) = desired_bundles.get(key) {
                wrote.insert(*i);
            }
        }
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
        .keyed_bundles()
        .into_iter()
        .filter(|b| !demanded_ids.contains(b.as_str()) && !held(b) && !ledger.has_entries_for(b))
        .collect();
    if allow_removals {
        for bundle in retire {
            ledger.retire_key(&bundle);
        }
    }

    out.warnings.extend(ledger.flush(io.fs, io.layout));

    out.bundles = demands
        .iter()
        .enumerate()
        .map(|(i, d)| BundleStates {
            bundle_id: d.bundle_id.clone(),
            states: states.remove(&i).unwrap_or_default(),
            wrote: wrote.contains(&i),
            first_placement: wrote.contains(&i) && !placed_before.contains(&d.bundle_id),
        })
        .collect();
    out
}

/// Surgically remove ONE bundle's placements from every engaged harness config in this scope —
/// the `remove` verb's inline convergence. Only the named bundle's entries move; everything else
/// in every file is left byte-identical. Drifted entries are LEFT and disclosed.
pub(crate) fn remove_bundle(
    io: &ScopeIo<'_>,
    descriptors: &[&'static KnownHarness],
    detected: &BTreeSet<String>,
    bundle_id: &str,
) -> ConvergeOutcome {
    let mut out = ConvergeOutcome::default();
    let _lock = match converge_lock(io) {
        Ok(guard) => guard,
        Err(warning) => {
            out.warnings.push(warning);
            return out;
        }
    };
    let mut ledger = match ScopeEntries::load(io.fs, io.layout) {
        Ok(l) => l,
        Err(e) => {
            out.warnings.push(format!(
                "MCP_LEDGER_UNREADABLE: {} — MCP entries are left in place",
                e.detail()
            ));
            return out;
        }
    };
    let dialect_of = config_custody::dialect_lookup(descriptors, io.project_root.is_some());
    ledger.recover(io.fs, &dialect_of);
    let Some(key) = ledger.key_of(bundle_id).map(str::to_owned) else {
        return out; // never placed here — nothing to converge
    };

    for h in descriptors {
        let surface: Option<(PathBuf, McpDialect)> = match &io.project_root {
            Some(root) => h.mcp().and_then(|m| m.project).and_then(|(rel, dialect)| {
                let path = root.join(rel);
                crate::placement::within_project(root, &path).then_some((path, dialect))
            }),
            None => h
                .mcp()
                .and_then(|m| m.user)
                .and_then(|s| h.mcp_user_path(&io.home).map(|p| (p, s.dialect))),
        };
        let Some((path, dialect)) = surface else {
            continue;
        };
        if !(detected.contains(h.slug) || io.fs.exists(&path)) {
            continue;
        }
        if !ledger.holds(&placement_key(h.slug, &key)) {
            continue; // nothing recorded on this surface
        }
        // Prior scoped to ONLY this bundle's key: the drivers remove prior-matched undesired
        // keys, and every other entry — ours or not — reads foreign and stays byte-identical.
        let only_this = |_l: &ScopeEntries, entry_key: &str| entry_key != key;
        let provenance = BTreeMap::new();
        let path = surface_file(&path, dialect);
        let surface_out = converge_file(
            io,
            &mut ledger,
            h,
            &path,
            dialect,
            &[],
            &only_this,
            &provenance,
        );
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
    }
    out.warnings.extend(ledger.flush(io.fs, io.layout));
    out
}

/// The per-scope MCP converge lock (`locks/mcp.lock`, blocking): every entry point that runs the
/// custody + config read-modify-write — the sweep's [`converge`], add's inline converge, a
/// targeted go-back's / accept's [`converge_bundle_now`], and [`remove_bundle`] — serializes on it, so two
/// processes can never interleave a read-modify-write over the same scope's configs.
///
/// LOCK ORDER, fixed: the sweep already holds `locks/currency.lock` when it converges, so this
/// lock is strictly INNER — taken only inside [`converge`]/[`remove_bundle`], released on return,
/// and NOTHING acquires another lock while holding it. No path takes it twice: the two holders
/// never call each other, and [`converge_bundle_now`] reads the ledger only ADVISORILY (deriving
/// its reach) before its one `converge` call takes the lock and re-reads authoritatively.
///
/// Failure is a refusal, not a fallback: without the lock nothing is read or written this run
/// (the warning says so), because an unserialized converge could interleave with another and
/// tear the ledger-vs-config agreement.
fn converge_lock(io: &ScopeIo<'_>) -> Result<crate::fs_seam::LockGuard, String> {
    let locks = io.layout.locks_dir();
    // Created only when absent so the common case adds no mutating op (the crash sweep counts
    // them).
    if !io.fs.exists(&locks)
        && let Err(e) = io.fs.create_dir_all(&locks)
    {
        return Err(format!(
            "MCP_LOCK_UNAVAILABLE: creating {} failed ({e}) — no MCP config is read or written \
             this run",
            locks.display()
        ));
    }
    io.fs.lock_exclusive(&locks.join("mcp.lock")).map_err(|e| {
        format!("MCP_LOCK_UNAVAILABLE: {e} — no MCP config is read or written this run")
    })
}

/// Whether a demand's dest narrowing admits `slug` (`None` = every MCP-capable harness; a dest
/// row admits exactly the harnesses its config files name — possibly none).
fn filter_admits(d: &McpDemand, slug: &str) -> bool {
    d.harness_filter
        .as_ref()
        .is_none_or(|f| f.iter().any(|s| s == slug))
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

/// ONE state vocabulary. The tokens are this engine's (see the module docs); the phrases are the
/// words a person reads for them — and every receipt that shows a per-agent outcome comes through
/// here, so `add` and `update` can never name the same state two different ways. The vocabulary
/// is OPEN: a token with no phrase of its own reads verbatim rather than being folded into a
/// wrong one.
pub(crate) fn state_phrase(state: &str) -> &str {
    match state {
        "placed" => "placed",
        "not-supported" => "not placed",
        other => other,
    }
}

// =================================================================================================
// One FILE surface (every dialect but the plugin dir): driver apply + the intent journal.
// =================================================================================================

/// What one surface's converge decided: per-KEY states (the caller joins keys back onto bundles),
/// the keys whose bytes this run actually WROTE into the config, whether the ledger moved, and the
/// surface's warnings.
struct SurfaceOutcome {
    states: Vec<(String, McpAgentState)>,
    /// The keys this surface WROTE this run — placed, updated, or repaired. Empty on a surface
    /// left byte-identical, which is what makes it an answer rather than a guess.
    wrote: BTreeSet<String>,
    warnings: Vec<String>,
}

impl SurfaceOutcome {
    fn empty() -> Self {
        Self {
            states: Vec::new(),
            wrote: BTreeSet::new(),
            warnings: Vec::new(),
        }
    }
    fn unprovable(desired: &[McpEntry], h: &KnownHarness, path: &Path, reason: &str) -> Self {
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
            wrote: BTreeSet::new(),
            warnings: vec![format!("MCP_SURFACE_UNPROVABLE {}: {reason}", h.slug)],
        }
    }
}

/// The intent-journal protocol around ONE config write: (a) the ledger persists the pending
/// intents, (b) the config file is replaced, (c) the ledger promotes the intents. A crash between
/// (b) and (c) is healed by [`McpLedger::recover_pending`] next run.
fn journaled_write(
    io: &ScopeIo<'_>,
    ledger: &mut ScopeEntries,
    path: &Path,
    intents: BTreeMap<String, PendingIntent>,
    write: &dyn Fn() -> std::io::Result<()>,
) -> Result<(), String> {
    if let Err(e) = ledger.journal(io.fs, io.layout, intents) {
        ledger.drop_journal(io.fs, io.layout);
        return Err(format!("the custody write failed ({})", e.detail()));
    }
    if let Err(e) = write() {
        // The config write did not land: drop the intents durably (best-effort — recovery would
        // reach the same answer by observing the file).
        ledger.drop_journal(io.fs, io.layout);
        return Err(format!("writing {} failed ({e})", path.display()));
    }
    // Promote: pending → each bundle's own rows, exactly as recovery would.
    ledger.promote_journal();
    let failures = ledger.flush(io.fs, io.layout);
    if !failures.is_empty() {
        // The rows moved in memory and the intents are still journaled ON DISK — the next run's
        // recovery promotes them again. Disclose, never lose.
        return Err(format!(
            "{}; recovery heals it next run",
            failures.join("; ")
        ));
    }
    Ok(())
}

/// One surface's converge, with the stale-row disclosure the ledger's file-scoped priors imply:
/// a row recorded at ANOTHER file (a surface path moved — e.g. an env-override change) is not a
/// prior here and is never dropped against this surface — it is warned about, naming the old
/// file, and left in place.
#[allow(clippy::too_many_arguments)]
fn converge_file(
    io: &ScopeIo<'_>,
    ledger: &mut ScopeEntries,
    h: &KnownHarness,
    path: &Path,
    dialect: McpDialect,
    desired: &[McpEntry],
    preserved: &dyn Fn(&ScopeEntries, &str) -> bool,
    provenance: &BTreeMap<String, (String, String)>,
) -> SurfaceOutcome {
    let stale: Vec<String> = ledger
        .stale_rows(h.slug, path)
        .into_iter()
        .map(|(key, old)| {
            format!(
                "MCP_ENTRY_STALE_PATH {}: {key} is recorded in {old}, but this scope's surface \
                 is {} — the old entry is left in place",
                h.slug,
                path.display()
            )
        })
        .collect();
    let mut out = converge_surface(io, ledger, h, path, dialect, desired, preserved, provenance);
    if !stale.is_empty() {
        out.warnings.splice(0..0, stale);
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn converge_surface(
    io: &ScopeIo<'_>,
    ledger: &mut ScopeEntries,
    h: &KnownHarness,
    path: &Path,
    dialect: McpDialect,
    desired: &[McpEntry],
    preserved: &dyn Fn(&ScopeEntries, &str) -> bool,
    provenance: &BTreeMap<String, (String, String)>,
) -> SurfaceOutcome {
    let mut prior = ledger.prior_for(h.slug, path);
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
    // The plugin dir is wholly topos-owned by construction, so content topos did not write is
    // not a mere loss of whole-file ownership — it is a takeover. Back the surface off whole:
    // unprovable, disclosed, byte-identical (a user's sibling key is never dropped by a rewrite,
    // and removal never deletes the file over it).
    let is_plugin = dialect == McpDialect::ClaudePluginDir;
    if is_plugin && unmanaged {
        return SurfaceOutcome::unprovable(
            desired,
            h,
            path,
            "the topos plugin dir holds content topos did not write",
        );
    }
    let outcome = mcp::apply(dialect, current.as_deref(), desired, &prior);
    match outcome.plan {
        EditPlan::Unprovable(reason) => SurfaceOutcome::unprovable(desired, h, path, &reason),
        EditPlan::Leave => {
            let mut surface = fold_states(h, path, &outcome.states, desired);
            sync_ledger_entries(
                ledger,
                h.slug,
                path,
                &outcome.fingerprints,
                &kept,
                provenance,
                false,
            );
            if unmanaged {
                ledger.clear_owns_file(h.slug, path);
            }
            // The plugin manifest is constant and carries no entry state: re-heal a hand-deleted
            // one beside entries that remain (best-effort, no journal needed). Healing it IS a
            // write — the entries beside it did not load until it was back — so every key on the
            // surface counts as written, exactly as it would had the entries file gone too.
            if is_plugin
                && !outcome.fingerprints.is_empty()
                && let Some(manifest) = plugin_manifest_path(path)
                && !io.fs.exists(&manifest)
                && crate::config_io::replace_config(io.fs, &manifest, &plugin_dir::manifest_bytes())
                    .is_ok()
            {
                // ONE truth on both channels: the keys that count as written say `placed`, the
                // state every reader joins on — a row whose verb reports a write while its own
                // per-agent line reads `current` has told a person two things.
                for (key, state) in &mut surface.states {
                    if state.state == "current"
                        && outcome.fingerprints.iter().any(|(k, _)| k == key)
                    {
                        state.state = "placed".to_owned();
                        state.note = h.mcp().map(|m| m.reload_note.to_owned());
                    }
                }
                surface
                    .wrote
                    .extend(outcome.fingerprints.iter().map(|(k, _)| k.clone()));
            }
            surface
        }
        EditPlan::Write(bytes) => {
            // Whole-file ownership for the NEXT ledger state: topos creates the file now, or it
            // owned every byte at the last write AND this reconcile saw nothing that is not ours
            // — neither a drifted/foreign managed entry NOR any unmanaged content.
            let owned_before = {
                let mine = ledger.rows_at(h.slug, path);
                !mine.is_empty() && mine.iter().all(|(_, e)| e.owns_file)
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
            // The plugin surface writes its constant manifest beside the entries file — before
            // it, so a crash never leaves entries without the manifest that makes them load. The
            // manifest rides the same foreign-occupant rule as every other byte: (re)written
            // only when absent or still byte-identical to what topos renders; a hand-edited one
            // is left standing, disclosed.
            let (manifest, mut manifest_kept) = if is_plugin {
                plugin_manifest_verdict(io.fs, h.slug, path)
            } else {
                (None, None)
            };
            let write = move || -> std::io::Result<()> {
                if let Some(m) = &manifest {
                    crate::config_io::replace_config(fs, m, &plugin_dir::manifest_bytes())?;
                }
                crate::config_io::replace_config(fs, &path_owned, &bytes)
            };
            match journaled_write(io, ledger, path, intents, &write) {
                Ok(()) => {
                    let mut surface = fold_states(h, path, &outcome.states, desired);
                    // The file topos wholly owned just lost its LAST entry: nothing of ours
                    // remains — delete it (through the seam; the write above already proved the
                    // path safe). CONSERVATIVE BELT before any whole-file delete: RE-READ the
                    // just-written post-image and delete ONLY when it is structurally empty —
                    // no managed entries left AND nothing beyond the fresh-creation skeleton.
                    // The ownership flag is a recorded claim; the post-image is the fact.
                    if desired.is_empty()
                        && owns_file
                        && ledger.rows_at(h.slug, path).is_empty()
                        && post_image_structurally_empty(io, dialect, path)
                        && io.fs.remove_file(path).is_ok()
                    {
                        if is_plugin {
                            // The prune re-answers the manifest question at the delete boundary
                            // — its disclosure (kept, dir stays) supersedes the write-time one.
                            manifest_kept = prune_plugin_dir(io.fs, h.slug, path);
                        }
                        surface.warnings.push(format!(
                            "MCP_FILE_REMOVED {}: {} held only topos entries and was deleted",
                            h.slug,
                            path.display()
                        ));
                    }
                    if let Some(kept) = manifest_kept {
                        surface.warnings.push(kept);
                    }
                    surface
                }
                Err(reason) => SurfaceOutcome::unprovable(desired, h, path, &reason),
            }
        }
    }
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
    ledger: &ScopeEntries,
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
                        .row(&placement_key(slug, key))
                        .map(|e| e.version_id.clone())
                        .unwrap_or_default();
                    (b.to_owned(), version)
                })
                .unwrap_or_default()
        })
    };
    for (key, fp) in &next {
        let ledger_key = placement_key(slug, key);
        let standing = ledger.row(&ledger_key);
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
    // A row recorded at ANOTHER file is the disclosed stale class — never removal-intended against
    // this surface (recovery would observe THIS file, read the key as absent, and drop custody of a
    // live entry elsewhere), so only THIS surface's rows are scanned.
    for (ledger_key, entry) in ledger.rows_at(slug, path) {
        if kept.contains(&entry.key) || next.contains_key(entry.key.as_str()) {
            continue;
        }
        let bundle_id = ledger
            .bundle_of_key(&entry.key)
            .unwrap_or_default()
            .to_owned();
        intents.insert(
            ledger_key,
            PendingIntent {
                bundle_id,
                version_id: String::new(),
                file: file.clone(),
                fingerprint: String::new(),
                owns_file: false,
            },
        );
    }
    intents
}

/// Sync the custody rows to a LEAVE outcome's fingerprints (no config write happened; the record
/// just tracks what the file provably holds — e.g. a prior key that vanished from the file).
#[allow(clippy::too_many_arguments)]
fn sync_ledger_entries(
    ledger: &mut ScopeEntries,
    slug: &str,
    path: &Path,
    fingerprints: &[(String, String)],
    kept: &BTreeSet<String>,
    provenance: &BTreeMap<String, (String, String)>,
    owns_file_default: bool,
) {
    let file = path.display().to_string();
    let next: BTreeMap<&str, &str> = fingerprints
        .iter()
        .map(|(k, f)| (k.as_str(), f.as_str()))
        .collect();
    // Drop standing rows the surface no longer carries (not the preserved ones, and never a row
    // recorded at ANOTHER file — that is the disclosed stale class, left in place).
    for (ledger_key, entry) in ledger.rows_at(slug, path) {
        if !next.contains_key(entry.key.as_str()) && !kept.contains(&entry.key) {
            ledger.remove(&ledger_key);
        }
    }
    for (key, fp) in &next {
        let ledger_key = placement_key(slug, key);
        let standing = ledger.row(&ledger_key).cloned();
        let (bundle, version) = provenance.get(*key).cloned().unwrap_or_else(|| {
            (
                ledger.bundle_of_key(key).unwrap_or_default().to_owned(),
                standing
                    .as_ref()
                    .map(|e| e.version_id.clone())
                    .unwrap_or_default(),
            )
        });
        let row = EntryPlacement {
            agent: slug.to_owned(),
            file: file.clone(),
            key: (*key).to_owned(),
            fingerprint: (**fp).to_owned(),
            owns_file: standing.as_ref().map_or(owns_file_default, |e| e.owns_file),
            version_id: version,
        };
        ledger.put(ledger_key, bundle, row);
    }
}

/// Fold driver per-key states into per-key wire states (see the module doc's vocabulary).
fn fold_states(
    h: &KnownHarness,
    path: &Path,
    states: &[(String, EntryState)],
    desired: &[McpEntry],
) -> SurfaceOutcome {
    let desired_keys: BTreeSet<&str> = desired.iter().map(|e| e.key.as_str()).collect();
    let mut out = SurfaceOutcome::empty();
    for (key, st) in states {
        let mapped = match st {
            // A NEW or CHANGED placement carries the harness's reload note — how the change goes
            // live. These two are also the whole of what "topos wrote this entry" means, so they
            // get their OWN wire state: a reader (the receipt, the fleet page, `--json`) can tell
            // a file this run wrote from one it merely found in order, and both channels say it
            // with the same word.
            EntryState::PlacedNew | EntryState::Updated => {
                out.wrote.insert(key.clone());
                agent_state(h.slug, "placed", h.mcp().map(|m| m.reload_note), Some(path))
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
// The Claude Code plugin dir — a wholly-topos-owned DIRECTORY whose `.mcp.json` is an ordinary
// driver surface; these helpers carry the parts the pure driver cannot know (the constant
// manifest beside it, the dir prune when the last entry leaves).
// =================================================================================================

/// The driver surface FILE for a resolved descriptor surface: the plugin DIR's `.mcp.json` for
/// [`McpDialect::ClaudePluginDir`], the path itself for every file dialect.
fn surface_file(path: &Path, dialect: McpDialect) -> PathBuf {
    if dialect == McpDialect::ClaudePluginDir {
        path.join(plugin_dir::PLUGIN_MCP_PATH)
    } else {
        path.to_path_buf()
    }
}

/// The constant manifest's path beside a plugin `.mcp.json`.
fn plugin_manifest_path(mcp_path: &Path) -> Option<PathBuf> {
    mcp_path
        .parent()
        .map(|dir| dir.join(plugin_dir::PLUGIN_MANIFEST_PATH))
}

/// Whether the constant manifest beside a plugin `.mcp.json` is topos's to write or delete:
/// `(Some(path), None)` when it is absent or still byte-identical to
/// [`plugin_dir::manifest_bytes`], else `(None, Some(disclosure))` — hand-edited (or unreadable,
/// so unprovable) bytes are a foreign occupant, left standing.
fn plugin_manifest_verdict(
    fs: &dyn FsOps,
    slug: &str,
    mcp_path: &Path,
) -> (Option<PathBuf>, Option<String>) {
    let Some(manifest) = plugin_manifest_path(mcp_path) else {
        return (None, None);
    };
    match fs.read_opt(&manifest) {
        Ok(None) => (Some(manifest), None),
        Ok(Some(bytes)) if bytes == plugin_dir::manifest_bytes() => (Some(manifest), None),
        _ => {
            let kept = format!(
                "MCP_PLUGIN_MANIFEST_KEPT {slug}: {} is not the manifest topos wrote — left in \
                 place",
                manifest.display()
            );
            (None, Some(kept))
        }
    }
}

/// Prune the plugin dir after its `.mcp.json` left: drop the constant manifest — only while its
/// bytes are still exactly [`plugin_dir::manifest_bytes`] — then each dir level that holds
/// nothing else: the whole-file ownership proof extended to the directory level, so a dir with
/// ANY foreign occupant is left standing. A hand-edited manifest is such an occupant — kept, the
/// dir kept with it, and the returned disclosure says so. Best-effort throughout (a stray empty
/// dir is recoverable; destroyed bytes are not).
fn prune_plugin_dir(fs: &dyn FsOps, slug: &str, mcp_path: &Path) -> Option<String> {
    let dir = mcp_path.parent()?;
    let manifest = dir.join(plugin_dir::PLUGIN_MANIFEST_PATH);
    let mut kept = None;
    match fs.read_opt(&manifest) {
        Ok(None) => {}
        Ok(Some(bytes)) if bytes == plugin_dir::manifest_bytes() => {
            let _ = fs.remove_file(&manifest);
        }
        _ => {
            kept = Some(format!(
                "MCP_PLUGIN_MANIFEST_KEPT {slug}: {} is not the manifest topos wrote — left in \
                 place, so the plugin dir stays",
                manifest.display()
            ));
        }
    }
    let empty = |d: &Path| fs.read_dir(d).is_ok_and(|entries| entries.is_empty());
    if let Some(manifest_dir) = manifest.parent()
        && manifest_dir != dir
        && fs.exists(manifest_dir)
        && empty(manifest_dir)
    {
        let _ = fs.remove_dir_all(manifest_dir);
    }
    if empty(dir) {
        let _ = fs.remove_dir_all(dir);
    }
    kept
}

/// Converge THIS scope's config entries for ONE bundle right now — the store/lock just moved (a
/// go-back, a targeted accept), and the command must not return success while agent configs still
/// carry the previous document. Same wiring as the sweep's converge, narrowed to one demand;
/// removals stay OFF (a targeted verb never touches another bundle's entries). Best-effort by construction: the store
/// move already landed, and the next sweep reaches the same configs — failures come back as
/// warning lines beside the per-agent states.
///
/// OWNED-ONLY CONVERGENCE: the targeted run does not re-derive the row's `dest` narrowing (that
/// lives in the scope plan the sweep resolves), so it must not fan out past what the narrowing
/// last admitted. Two rails hold that line:
///
/// - it runs only when the ledger holds this bundle's MINTED key — with the ledger gone there is
///   no ownership record to reuse, and minting fresh here would land a DUPLICATE `topos-local-*`
///   entry beside the original (now-foreign, unremovable) one. Skip with an honest warning; the
///   next sweep re-mints under the full scope plan and heals this scope;
/// - it converges only the harnesses that ALREADY hold a ledger entry for this bundle in this
///   scope. A harness the narrowing excluded never gained an entry, so it stays untouched; a
///   narrowing CHANGE is the sweep's job, not a go-back's.
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
    // An ADVISORY (unlocked) read: it only derives the targeted reach below. The authoritative
    // read-modify-write happens inside `converge`, under the per-scope converge lock.
    let ledger = match ScopeEntries::load(ctx.fs, &ctx.layout) {
        Ok(l) => l,
        Err(e) => {
            // The same fail-closed answer the sweep's converge gives an unreadable ledger.
            return (
                Vec::new(),
                vec![format!(
                    "MCP_LEDGER_UNREADABLE: {} — no MCP config is read or written this run",
                    e.detail()
                )],
            );
        }
    };
    let Some(key) = ledger.key_of(sid.as_str()).map(str::to_owned) else {
        return (
            Vec::new(),
            vec![format!(
                "MCP_OWNERSHIP_MISSING {name}: ownership record missing here — the next update \
                 heals this scope"
            )],
        );
    };
    let descriptors = mcp::descriptor::mcp_harnesses();
    // The harnesses that provably hold this bundle's entry here — the whole reach of a targeted
    // converge. None ⇒ nothing placed, nothing stale, nothing to do.
    let owned: Vec<String> = descriptors
        .iter()
        .filter(|h| ledger.holds(&placement_key(h.slug, &key)))
        .map(|h| h.slug.to_owned())
        .collect();
    if owned.is_empty() {
        return (Vec::new(), Vec::new());
    }
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
    // `workspace_slug: None` is safe: the key check above proved the mint will find and reuse the
    // existing key, never build a fresh one from the namespace rule.
    let demand = McpDemand {
        bundle_id: sid.as_str().to_owned(),
        name: name.to_owned(),
        workspace_slug: None,
        version_id,
        server_json,
        harness_filter: Some(owned),
    };
    let outcome = converge(
        &io,
        std::slice::from_ref(&demand),
        &descriptors,
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
