//! The per-scope MCP CONVERGENCE engine — how a `kind = "mcp"` bundle's `server.json` becomes (and
//! stops being) an entry in each detected agent's own MCP config.
//!
//! The pure placement math lives in `topos_harness::mcp` (bytes in → an [`EditPlan`] out, one
//! driver per dialect); THIS module owns everything stateful around it, per scope:
//!
//! - the demand set — what the reconcile resolved this run, PLANNED onto this scope's config
//!   surfaces ([`McpDemand`], built only by [`DemandedBundle::planned`]). Reach is the plan's
//!   answer: the converge places a bundle exactly where its plan carries an entries target, and
//!   states the surfaces the plan withheld. Removal and key retirement read the RECORDED rows
//!   instead — a bundle being removed has no demand left to plan from, and custody is the only
//!   thing that knows where its entries went,
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
//! - removal convergence: custody entries whose bundle is no longer demanded leave through the
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

use crate::config_custody::EntryPlacement;
use crate::config_custody::{self, PendingIntent, ScopeEntries, placement_key};
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::placement::PlacementPlan;
use crate::sidecar::Layout;

/// What a caller RESOLVED about one demanded MCP bundle, before its placement is planned: the
/// identity, the bytes, and the reach its row asked for. Turned into an [`McpDemand`] — the only
/// way one is ever built — by [`DemandedBundle::planned`].
#[derive(Debug, Clone)]
pub(crate) struct DemandedBundle {
    /// The bundle identity the custody keys on (the workspace skill id; `local:<name>` for an
    /// untracked local row).
    pub bundle_id: String,
    /// The catalog / row name (the key-mint ingredient and the receipt name).
    pub name: String,
    /// The workspace address slug for a workspace bundle (`None` = a local row) — the key-mint
    /// namespace.
    pub workspace_slug: Option<String>,
    /// The stored version the `server.json` bytes came from (custody provenance).
    pub version_id: String,
    /// The bundle's `server.json` bytes, read from the scope store's current tree (or the local
    /// row's dir).
    pub server_json: Vec<u8>,
    /// The harness narrowing the caller resolved — the slugs whose config files the row's `dest`
    /// names, or (for a targeted verb) the harnesses whose recorded rows prove the bundle already
    /// stands there. `None` = every MCP-capable harness. It is a PLANNER INPUT and nothing else:
    /// the plan turns it into targets, and no downstream step re-derives reach.
    pub reach: Option<Vec<String>>,
}

impl DemandedBundle {
    /// Plan this row onto ONE scope's config surfaces — the ONE construction of an [`McpDemand`].
    /// The scope (its fs, home and project root) comes from `io`, and `detected` is the SAME set
    /// the converge this feeds engages against.
    pub(crate) fn planned(
        self,
        io: &ScopeIo<'_>,
        descriptors: &[&'static KnownHarness],
        detected: &BTreeSet<String>,
    ) -> McpDemand {
        let plan = crate::placement::entries_plan_at(
            io.fs,
            descriptors,
            &io.home,
            detected,
            io.project_root.as_deref(),
            self.reach.as_deref(),
        );
        McpDemand {
            bundle_id: self.bundle_id,
            name: self.name,
            workspace_slug: self.workspace_slug,
            version_id: self.version_id,
            server_json: self.server_json,
            plan,
        }
    }
}

/// One demanded MCP bundle PLANNED onto one scope — the identity + bytes the caller resolved, and
/// the plan that says WHERE its entries belong here. The converge is demand (these plans) plus
/// custody (each bundle's own recorded rows); it never re-decides reach.
#[derive(Debug, Clone)]
pub(crate) struct McpDemand {
    /// See [`DemandedBundle::bundle_id`].
    pub bundle_id: String,
    /// See [`DemandedBundle::name`].
    pub name: String,
    /// See [`DemandedBundle::workspace_slug`].
    pub workspace_slug: Option<String>,
    /// See [`DemandedBundle::version_id`].
    pub version_id: String,
    /// See [`DemandedBundle::server_json`].
    pub server_json: Vec<u8>,
    /// The ENTRIES half of this bundle's placement plan at this scope: one target per config file
    /// its entries belong in, plus the surfaces the plan withheld with their reasons.
    pub plan: PlacementPlan,
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
    /// placement is an INSTALL however the store got its bytes; a write over a bundle the custody
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
/// harness's config; custody entries whose bundle is neither demanded nor held leave — but only
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
    let mut custody = match ScopeEntries::load(io.fs, io.layout) {
        Ok(l) => l,
        Err(e) => {
            out.warnings.push(format!(
                "MCP_CUSTODY_UNREADABLE {}: {} — no MCP config is read or written this run \
                 (without a decipherable custody document no entry's ownership is answerable, and \
                 an empty answer would read every managed entry as foreign)",
                io.layout.config_custody_path().display(),
                e.detail()
            ));
            return out;
        }
    };

    // Crash recovery over the intent journal, BEFORE any prior map is built (a landed-but-
    // unpromoted write would otherwise read as user drift forever).
    let dialect_of = config_custody::dialect_lookup(descriptors, io.project_root.is_some());
    if custody.recover(io.fs, &dialect_of) {
        let failures = custody.flush(io.fs, io.layout);
        if !failures.is_empty() {
            out.warnings.extend(failures);
            out.warnings.push(
                "MCP_CUSTODY_WRITE_FAILED: recovery could not be recorded — MCP convergence \
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
    let placed_before: HashSet<String> = custody.placed_bundles().into_iter().collect();

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
                custody.mint_key(&d.bundle_id, &d.name, d.workspace_slug.as_deref()),
            )
        })
        .collect();

    // Per-bundle state collector, keyed by demand index (order preserved for the receipt).
    let mut states: BTreeMap<usize, Vec<McpAgentState>> = BTreeMap::new();
    // The demands whose config entries this run WROTE somewhere — see [`BundleStates::wrote`].
    let mut wrote: BTreeSet<usize> = BTreeSet::new();

    for h in descriptors {
        // WHAT THE PLANS WITHHELD from this surface — one line per placeable demand that asked for
        // this harness and did not get it (no surface at this scope, a project path the containment
        // rail refused). The reach decision is the plan's, so the disclosure of what it cost is the
        // plan's too; the converge only speaks it in the descriptor order the receipt reads in.
        for (i, _) in &parsed {
            if let Some(w) = demands[*i].plan.withheld_for(h.slug) {
                push_state(
                    &mut states,
                    *i,
                    agent_state(h.slug, w.state, Some(w.note.as_str()), None),
                );
            }
        }
        // WHERE this surface's file is. A harness some plan REACHES answers with the plan's own
        // target — the file the demand named, and engagement already proven when it was planned.
        // A harness only CUSTODY names (an undemanded bundle's standing entry, a narrowing change,
        // a removal-only run) has no plan to read, so it resolves through the same shared
        // resolution the planner used — because a removal must still reach the files it wrote.
        let planned = parsed
            .iter()
            .find_map(|(i, _)| demands[*i].plan.entries_for(h.slug));
        let (file, dialect) = match planned {
            Some(t) => (t.file.clone(), t.dialect),
            None => {
                let surface =
                    crate::placement::config_surface(h, &io.home, io.project_root.as_deref());
                let crate::placement::ConfigSurface::Ready {
                    root,
                    file,
                    dialect,
                } = surface
                else {
                    // A project surface the containment rail refused earns ONE scope-level
                    // disclosure (the per-bundle states rode the plans above); a harness with no
                    // surface at this scope is simply not here.
                    if let crate::placement::ConfigSurface::Escaped { path } = surface {
                        let line = crate::placement::escape_line(h.slug, &path);
                        if !out.warnings.contains(&line) {
                            out.warnings.push(line);
                        }
                    }
                    continue;
                };
                // Engagement: the harness is detected on this machine, OR its config surface
                // already exists (entries were placed while it was detected — removal must still
                // reach them).
                if !(detected.contains(h.slug) || io.fs.exists(&root)) {
                    continue;
                }
                (file, dialect)
            }
        };

        // The desired set for this harness: the placeable demands whose PLAN puts an entries
        // target here. Nothing is re-narrowed — the plan already answered.
        let mut desired: Vec<McpEntry> = Vec::new();
        let mut desired_bundles: BTreeMap<String, usize> = BTreeMap::new();
        for (i, p) in &parsed {
            if demands[*i].plan.entries_for(h.slug).is_none() {
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
            if demands[*i].plan.entries_for(h.slug).is_some() {
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
        let preserved = |custody: &ScopeEntries, entry_key: &str| -> bool {
            let Some(bundle) = custody.bundle_of_key(entry_key) else {
                return false;
            };
            if demanded_ids.contains(bundle) && !held(bundle) {
                return false;
            }
            held(bundle) || !allow_removals
        };

        // Provenance for the custody rows this surface may write: key → (bundle, version).
        let provenance: BTreeMap<String, (String, String)> = desired_bundles
            .iter()
            .map(|(key, i)| {
                let d = &demands[*i];
                (key.clone(), (d.bundle_id.clone(), d.version_id.clone()))
            })
            .collect();
        // The plugin dir's driver surface is its `.mcp.json` (resolved with the surface); the
        // manifest beside it and the dir prune are `converge_file`'s dialect-specific I/O.
        let surface_out = converge_file(
            io,
            &mut custody,
            h,
            &file,
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
                        && let Some(bundle) = custody.bundle_of_key(&key)
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
    let retire: Vec<String> = custody
        .keyed_bundles()
        .into_iter()
        .filter(|b| !demanded_ids.contains(b.as_str()) && !held(b) && !custody.has_entries_for(b))
        .collect();
    if allow_removals {
        for bundle in retire {
            custody.retire_key(&bundle);
        }
    }

    out.warnings.extend(custody.flush(io.fs, io.layout));

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
    let mut custody = match ScopeEntries::load(io.fs, io.layout) {
        Ok(l) => l,
        Err(e) => {
            out.warnings.push(format!(
                "MCP_CUSTODY_UNREADABLE {}: {} — MCP entries are left in place",
                io.layout.config_custody_path().display(),
                e.detail()
            ));
            return out;
        }
    };
    // Crash recovery FIRST, and made DURABLE before anything else touches the journal: the
    // removal below journals its own intents, and `journal` replaces `pending` wholesale. A
    // recovery left only in memory would be overwritten by that write and the crashed run's
    // outstanding intent lost — so the promotion lands on disk here or this run does nothing.
    let dialect_of = config_custody::dialect_lookup(descriptors, io.project_root.is_some());
    if custody.recover(io.fs, &dialect_of) {
        let failures = custody.flush(io.fs, io.layout);
        if !failures.is_empty() {
            out.warnings.extend(failures);
            out.warnings.push(
                "MCP_CUSTODY_WRITE_FAILED: recovery could not be recorded — MCP entries are left \
                 in place"
                    .to_owned(),
            );
            return out;
        }
    }
    let Some(key) = custody.key_of(bundle_id).map(str::to_owned) else {
        return out; // never placed here — nothing to converge
    };

    for h in descriptors {
        // A removal resolves its surfaces from the DESCRIPTOR table and its reach from the
        // RECORDED rows — never from a plan. A bundle being removed has no demand left to plan
        // from, and custody is the only thing that knows where its entries actually went.
        let crate::placement::ConfigSurface::Ready {
            root,
            file,
            dialect,
        } = crate::placement::config_surface(h, &io.home, io.project_root.as_deref())
        else {
            continue;
        };
        if !(detected.contains(h.slug) || io.fs.exists(&root)) {
            continue;
        }
        if !custody.holds(&placement_key(h.slug, &key)) {
            continue; // nothing recorded on this surface
        }
        // Prior scoped to ONLY this bundle's key: the drivers remove prior-matched undesired
        // keys, and every other entry — ours or not — reads foreign and stays byte-identical.
        let only_this = |_l: &ScopeEntries, entry_key: &str| entry_key != key;
        let provenance = BTreeMap::new();
        let surface_out = converge_file(
            io,
            &mut custody,
            h,
            &file,
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

    if !custody.has_entries_for(bundle_id) {
        custody.retire_key(bundle_id);
    }
    out.warnings.extend(custody.flush(io.fs, io.layout));
    out
}

/// Move a bundle's SURVIVING config rows out of its record, under the converge lock, because the
/// record is about to be deleted (see [`crate::config_custody::detach_to_unrecorded`]). Returns the
/// caller's warning lines — best-effort, because the removal itself already landed.
pub(crate) fn detach_bundle_rows(io: &ScopeIo<'_>, bundle_id: &str) -> Vec<String> {
    let _lock = match converge_lock(io) {
        Ok(guard) => guard,
        Err(warning) => return vec![warning],
    };
    match config_custody::detach_to_unrecorded(io.fs, io.layout, bundle_id) {
        Ok(_) => Vec::new(),
        Err(e) => vec![format!(
            "MCP_CUSTODY_WRITE_FAILED {bundle_id}: {} — a config entry left standing here is no \
             longer tracked",
            e.detail()
        )],
    }
}

/// The per-scope MCP converge lock (`locks/mcp.lock`, blocking): every entry point that runs the
/// custody + config read-modify-write — the sweep's [`converge`], add's inline converge, a
/// targeted go-back's / accept's [`converge_bundle_now`], and [`remove_bundle`] — serializes on it, so two
/// processes can never interleave a read-modify-write over the same scope's configs.
///
/// LOCK ORDER, fixed: the sweep already holds `locks/currency.lock` when it converges, so this
/// lock is strictly INNER — taken only inside [`converge`]/[`remove_bundle`], released on return,
/// and NOTHING acquires another lock while holding it. No path takes it twice: the two holders
/// never call each other, and [`converge_bundle_now`] reads custody only ADVISORILY (deriving its
/// reach) before its one `converge` call takes the lock and re-reads authoritatively.
///
/// **This lock does NOT serialize against the per-skill flock, and does not need to.** The two lock
/// domains own DIFFERENT FILES, and every file has exactly one writer: `map.json` (a bundle's dir
/// custody) is written only under `lock_skill`, `entries.json` (the same bundle's config custody)
/// and `state/config_custody.json` only under this one. That is why a targeted verb may hold the
/// skill lock across a converge — the nesting is skill → mcp, never the reverse, and the inner
/// domain writes nothing the outer one is holding a snapshot of. Putting both custody halves in one
/// document would make that nesting a correctness requirement instead of a convenience: a verb that
/// read `map.json` before a concurrent converge and wrote it after would silently drop the rows
/// that converge had just committed, leaving the entry in the file with no record — permanently
/// drifted. READS join the two files lock-free, as snapshots, exactly as they always have.
///
/// Failure is a refusal, not a fallback: without the lock nothing is read or written this run
/// (the warning says so), because an unserialized converge could interleave with another and
/// tear the custody-vs-config agreement.
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
/// the keys whose bytes this run actually WROTE into the config, and the surface's warnings.
/// Custody movement is not reported here — the rows live on [`ScopeEntries`], which tracks its own
/// dirty set and persists it in one `flush`.
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

/// The intent-journal protocol around ONE config write: (a) the custody persists the pending
/// intents, (b) the config file is replaced, (c) the custody promotes the intents. A crash between
/// (b) and (c) is healed by [`McpLedger::recover_pending`] next run.
fn journaled_write(
    io: &ScopeIo<'_>,
    custody: &mut ScopeEntries,
    path: &Path,
    intents: BTreeMap<String, PendingIntent>,
    write: &dyn Fn() -> std::io::Result<()>,
) -> Result<(), String> {
    // ONE journal, and journalling REPLACES it. So a surface may only journal into an EMPTY one:
    // intents standing here belong to an earlier surface whose record write failed (or to a config
    // write whose outcome is unknown), and they are the only description of work that has not
    // landed. Writing over them would leave that entry live in its config file with no row and
    // nothing to recover from — permanently. Refusing costs this surface one run; the next run's
    // recovery resolves the outstanding intents by OBSERVING the files and this surface converges
    // normally after it. Merging instead would be worse than it looks: `promote_journal` applies
    // intents blindly (correct only for the write this process just performed), so it would record
    // custody for another surface's bytes that may never have been written.
    if custody.has_pending() {
        return Err(format!(
            "earlier config work is not yet durable — {} is skipped this run; the next run's \
             recovery finishes it",
            path.display()
        ));
    }
    if let Err(e) = custody.journal(io.fs, io.layout, intents) {
        custody.drop_journal(io.fs, io.layout);
        return Err(format!("the custody write failed ({})", e.detail()));
    }
    if let Err(e) = write() {
        // The config write FAILED — which is not the same as "nothing landed". A replace is
        // several syscalls, and an error after the rename leaves the new bytes in the file. So the
        // intents STAY journaled: the journal's whole purpose is to be resolved by OBSERVING the
        // file, and next run's recovery promotes what actually landed and drops what did not.
        // Clearing them here on the assumption that the error means "no bytes moved" is exactly
        // how a live entry ends up with no record — permanently unremovable, read as a hand edit
        // by every run afterwards. One extra recovery pass is the whole cost of not guessing.
        return Err(format!("writing {} failed ({e})", path.display()));
    }
    // Promote: pending → each bundle's own rows, exactly as recovery would.
    custody.promote_journal();
    let failures = custody.flush(io.fs, io.layout);
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

/// One surface's converge, with the stale-row disclosure the custody's file-scoped priors imply:
/// a row recorded at ANOTHER file (a surface path moved — e.g. an env-override change) is not a
/// prior here and is never dropped against this surface — it is warned about, naming the old
/// file, and left in place.
#[allow(clippy::too_many_arguments)]
fn converge_file(
    io: &ScopeIo<'_>,
    custody: &mut ScopeEntries,
    h: &KnownHarness,
    path: &Path,
    dialect: McpDialect,
    desired: &[McpEntry],
    preserved: &dyn Fn(&ScopeEntries, &str) -> bool,
    provenance: &BTreeMap<String, (String, String)>,
) -> SurfaceOutcome {
    let stale: Vec<String> = custody
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
    let mut out = converge_surface(
        io, custody, h, path, dialect, desired, preserved, provenance,
    );
    if !stale.is_empty() {
        out.warnings.splice(0..0, stale);
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn converge_surface(
    io: &ScopeIo<'_>,
    custody: &mut ScopeEntries,
    h: &KnownHarness,
    path: &Path,
    dialect: McpDialect,
    desired: &[McpEntry],
    preserved: &dyn Fn(&ScopeEntries, &str) -> bool,
    provenance: &BTreeMap<String, (String, String)>,
) -> SurfaceOutcome {
    let mut prior = custody.prior_for(h.slug, path);
    let kept: BTreeSet<String> = prior
        .keys()
        .filter(|k| preserved(custody, k))
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
                custody,
                h.slug,
                path,
                &outcome.fingerprints,
                &kept,
                provenance,
                false,
            );
            if unmanaged {
                custody.clear_owns_file(h.slug, path);
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
            // Whole-file ownership for the NEXT custody state: topos creates the file now, or it
            // owned every byte at the last write AND this reconcile saw nothing that is not ours
            // — neither a drifted/foreign managed entry NOR any unmanaged content.
            let owned_before = {
                let mine = custody.rows_at(h.slug, path);
                !mine.is_empty() && mine.iter().all(|(_, e)| e.owns_file)
            };
            // The same drift question the dir side asks of a folder, asked of the entries —
            // through the one vocabulary both project onto.
            let all_ours = outcome.states.iter().all(|(_, s)| {
                !matches!(
                    crate::placement::Drift::of_entry(*s),
                    crate::placement::Drift::Modified | crate::placement::Drift::Foreign
                )
            });
            let owns_file =
                outcome.created_file || (owned_before && all_ours && kept.is_empty() && !unmanaged);

            let intents = write_intents(
                custody,
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
            match journaled_write(io, custody, path, intents, &write) {
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
                        && custody.rows_at(h.slug, path).is_empty()
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
    custody: &ScopeEntries,
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
            custody
                .bundle_of_key(key)
                .map(|b| {
                    let version = custody
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
        let standing = custody.row(&ledger_key);
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
    for (ledger_key, entry) in custody.rows_at(slug, path) {
        if kept.contains(&entry.key) || next.contains_key(entry.key.as_str()) {
            continue;
        }
        let bundle_id = custody
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
    custody: &mut ScopeEntries,
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
    for (ledger_key, entry) in custody.rows_at(slug, path) {
        if !next.contains_key(entry.key.as_str()) && !kept.contains(&entry.key) {
            custody.remove(&ledger_key);
        }
    }
    for (key, fp) in &next {
        let ledger_key = placement_key(slug, key);
        let standing = custody.row(&ledger_key).cloned();
        let (bundle, version) = provenance.get(*key).cloned().unwrap_or_else(|| {
            (
                custody.bundle_of_key(key).unwrap_or_default().to_owned(),
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
        custody.put(ledger_key, bundle, row);
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
/// - it runs only when the custody holds this bundle's MINTED key — with the custody gone there is
///   no ownership record to reuse, and minting fresh here would land a DUPLICATE `topos-local-*`
///   entry beside the original (now-foreign, unremovable) one. Skip with an honest warning; the
///   next sweep re-mints under the full scope plan and heals this scope;
/// - `plan` — built by [`recorded_entries_plan`] — reaches only the harnesses that ALREADY hold a
///   custody entry for this bundle in this scope. A harness the narrowing excluded never gained an
///   entry, so it stays untouched; a narrowing CHANGE is the sweep's job, not a go-back's.
pub(crate) fn converge_bundle_now(
    ctx: &crate::ctx::Ctx<'_>,
    sid: &crate::id::SkillId,
    name: &str,
    plan: &PlacementPlan,
) -> (Vec<McpAgentState>, Vec<String>) {
    let Some(roots) = ctx.roots.clone() else {
        return (Vec::new(), Vec::new());
    };
    let Ok(Some((version_id, server_json))) = stored_server_json(ctx, sid) else {
        return (Vec::new(), Vec::new());
    };
    // An ADVISORY (unlocked) read: it only answers whether an ownership record exists to reuse.
    // The authoritative read-modify-write happens inside `converge`, under the per-scope lock.
    let custody = match ScopeEntries::load(ctx.fs, &ctx.layout) {
        Ok(l) => l,
        Err(e) => {
            // The same fail-closed answer the sweep's converge gives an unreadable custody.
            return (
                Vec::new(),
                vec![format!(
                    "MCP_CUSTODY_UNREADABLE {}: {} — no MCP config is read or written this run",
                    ctx.layout.config_custody_path().display(),
                    e.detail()
                )],
            );
        }
    };
    if custody.key_of(sid.as_str()).is_none() {
        return (
            Vec::new(),
            vec![format!(
                "MCP_OWNERSHIP_MISSING {name}: ownership record missing here — the next update \
                 heals this scope"
            )],
        );
    }
    // Nothing planned ⇒ nothing placed here, nothing stale, nothing to do.
    if plan.entries().next().is_none() {
        return (Vec::new(), Vec::new());
    }
    let descriptors = mcp::descriptor::mcp_harnesses();
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
        plan: plan.clone(),
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

/// **The targeted verbs' ENTRIES plan** — the row-derived reach A2 keeps: a bundle's plan here is
/// exactly the harnesses whose RECORDED rows prove it already stands there, planned onto this
/// scope's surfaces through the one planner. A targeted accept, go-back or reset must never fan a
/// bundle out past what the sweep's narrowing last admitted, and the record is the only local
/// evidence of what that was. An unreadable custody, or a bundle with no minted key, plans nothing
/// — `converge_bundle_now` then says so with its own honest warning.
pub(crate) fn recorded_entries_plan(
    ctx: &crate::ctx::Ctx<'_>,
    skill_id: &str,
) -> crate::placement::PlacementPlan {
    let owned: Vec<String> = ScopeEntries::load(ctx.fs, &ctx.layout)
        .ok()
        .and_then(|custody| {
            let key = custody.key_of(skill_id)?.to_owned();
            Some(
                mcp::descriptor::mcp_harnesses()
                    .iter()
                    .filter(|h| custody.holds(&placement_key(h.slug, &key)))
                    .map(|h| h.slug.to_owned())
                    .collect(),
            )
        })
        .unwrap_or_default();
    crate::placement::entries_plan(ctx, ctx.layout.project_root(), Some(&owned))
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
