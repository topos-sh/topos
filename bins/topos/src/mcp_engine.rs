//! The per-scope MCP CONVERGENCE engine — how a `kind = "mcp"` bundle's `server.json` becomes (and
//! stops being) an entry in each picked agent's own MCP config.
//!
//! The pure placement math lives in `topos_harness::mcp` (bytes in → an [`EditPlan`] out, one
//! driver per dialect); THIS module owns everything stateful around it, per scope:
//!
//! - the COLLISION pre-flight ([`collisions`]): before a surface is written, what already stands
//!   where each entry would go — by name or by the server it points at — in the dest file AND in
//!   the read-only files the harness also reads servers from (the table's conflict paths). An
//!   entry topos does not own blocks that one placement, is never touched, and is reported with
//!   the way out,
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
//! - the per-harness surface: the descriptor table joined onto the agents pick (a harness engages
//!   only when its slug is picked OR its config file already exists — the second arm is custody:
//!   an entry placed while the agent was picked must still be reachable for removal), the
//!   person/project surface
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
//! ONE demand field is not a document fact: the SESSION a workspace delivery ran under
//! ([`DemandedBundle::gateway`]). A workspace can share a server it reaches on the agent's behalf,
//! and the entry for one is dialed with this machine's own credential for that workspace — so the
//! credential is carried beside the bytes, never inside them, and it is honoured only where the
//! demand's own provenance vouches for the document's claim ([`McpDemand::gateway_bearer`]). A
//! local folder row carries none and can earn none.
//!
//! Wire states (an OPEN vocabulary, kept small): `placed` (THIS run wrote the entry — a first
//! placement, an update to it, or the repair of one that was gone) · `current` (found already in
//! order; nothing written) · `drifted` (hand-edited since topos wrote it — untouched) ·
//! `not-supported` (withheld: capability or no surface at this scope) · `unprovable` (the surface
//! cannot be safely edited) · `conflicting` (an entry topos does not own already stands where this
//! one would go — the same config key, or the same server under another name, here or in a file
//! the harness also reads) · `removed` (removal receipts only).
//!
//! `conflicting` is DECIDED EVERY RUN from what the files hold, never stored: it appears the sweep
//! after somebody else's entry does, and disappears the sweep after they remove it, when the
//! placement lands normally.
//!
//! `placed` vs `current` is the ONE fact both channels answer with: the JSON state and the words a
//! person reads come from the same [`EntryState`], so a receipt can never say a file was written
//! while the wire calls it merely current.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use topos_harness::mcp::{self, EditPlan, EntryState, McpDialect, McpEntry, plugin_dir};
use topos_harness::registry::KnownHarness;
use topos_types::Message;
use topos_types::results::{McpAgentState, TargetOutcome};

use crate::config_custody::EntryPlacement;
use crate::config_custody::{self, PendingIntent, ScopeEntries, placement_key};
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::mcp_render::ServerDoc;
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
    /// Where the `server.json` bytes came from, recorded on every entry this demand places
    /// (custody provenance): the catalog revision (`mcpr_…`) for a workspace server, empty for a
    /// local row, whose folder IS the provenance.
    pub version_id: String,
    /// The bundle's `server.json` bytes — the record this scope holds for a workspace server, or
    /// the local row's own file.
    pub server_json: Vec<u8>,
    /// The harness narrowing the caller resolved — the slugs whose config files the row's `dest`
    /// names, or (for a targeted verb) the harnesses whose recorded rows prove the bundle already
    /// stands there. `None` = every MCP-capable harness. It is a PLANNER INPUT and nothing else:
    /// the plan turns it into targets, and no downstream step re-derives reach.
    pub reach: Option<Vec<String>>,
    /// **The session this delivery ran under**, for the one document shape that needs one: a
    /// workspace that reaches the server on the agent's behalf delivers a document saying so, and
    /// the entry is then dialed with this machine's own credential for THAT workspace
    /// ([`crate::mcp_render::select`]).
    ///
    /// `None` for every demand that did not come from a live workspace delivery — a local folder
    /// row above all. The pairing is what makes the claim safe: the credential a demand carries is
    /// the one its own session minted, so a document can never name an address of its choosing and
    /// be handed the credential for a workspace it does not belong to. See
    /// [`McpDemand::gateway_bearer`], the one place the two halves are joined.
    pub gateway: Option<crate::mcp_render::GatewayBearer>,
}

impl DemandedBundle {
    /// Plan this row onto ONE scope's config surfaces — the ONE construction of an [`McpDemand`].
    /// The scope (its fs, home and project root) comes from `io`, and `picked` is the SAME set
    /// the converge this feeds engages against.
    pub(crate) fn planned(
        self,
        io: &ScopeIo<'_>,
        descriptors: &[&'static KnownHarness],
        picked: &BTreeSet<String>,
    ) -> McpDemand {
        let plan = crate::placement::entries_plan_at(
            descriptors,
            &io.home,
            picked,
            io.project_root.as_deref(),
            self.reach.as_deref(),
        );
        McpDemand {
            bundle_id: self.bundle_id,
            name: self.name,
            workspace_slug: self.workspace_slug,
            version_id: self.version_id,
            server_json: self.server_json,
            gateway: self.gateway,
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
    /// See [`DemandedBundle::gateway`]. Read only through [`Self::gateway_bearer`].
    pub gateway: Option<crate::mcp_render::GatewayBearer>,
    /// The ENTRIES half of this bundle's placement plan at this scope: one target per config file
    /// its entries belong in, plus the surfaces the plan withheld with their reasons.
    pub plan: PlacementPlan,
}

impl McpDemand {
    /// **The credential this demand may be rendered with — and the ONE place the two halves of
    /// that question are asked together.** A gateway claim is honoured only for a demand that came
    /// from a workspace (it carries that workspace's slug) AND carries the credential its own
    /// session minted; anything else answers `None`, and a document making the claim is refused
    /// rather than dialed bare ([`crate::mcp_render::Gap::NoSession`]).
    ///
    /// The workspace half is what a LOCAL FOLDER row can never satisfy: its demand carries no
    /// slug, so a `server.json` in a folder cannot spell the claim into a credential — which is
    /// the whole point, since the address in that file would be the folder's own choosing.
    fn gateway_bearer(&self) -> Option<&crate::mcp_render::GatewayBearer> {
        self.workspace_slug.as_ref()?;
        self.gateway.as_ref()
    }
}

/// The scope's I/O: the fs seam, the scope store (where the custody document and every bundle's own
/// record live), the machine roots the surfaces resolve against, and the probe that answers whether
/// a runtime a placement needs is on this machine.
pub(crate) struct ScopeIo<'a> {
    pub fs: &'a dyn FsOps,
    /// Whether the runtime a program-run server would need (`npx`, `uvx`) can be found here. On
    /// the seam because "not placed: needs node" must be a TESTED outcome and not a property of
    /// the machine running the suite.
    pub runtimes: &'a dyn crate::mcp_render::RuntimeProbe,
    /// The program a gateway-routed entry runs (`<this binary> relay <url>`) — resolved by the
    /// caller ([`relay_program`] in production) and on the seam so a rendered command is a TESTED
    /// byte sequence, not the path of whichever binary ran the suite.
    pub relay_program: String,
    /// The scope's store layout — where `state/config_custody.json` and the per-bundle records live.
    pub layout: &'a Layout,
    /// The machine home (user surfaces resolve under it).
    pub home: PathBuf,
    /// `Some` = the PROJECT scope: surfaces are the project-relative ones, containment-proven.
    pub project_root: Option<PathBuf>,
}

/// The program a relay entry names in production: THIS binary, by its absolute path — the entry
/// is spawned by a harness whose `PATH` nobody here controls. The bare name is the honest
/// second-best when the path cannot be read or spelled (it resolves on the harness's own `PATH`,
/// where every install script puts the binary anyway).
pub(crate) fn relay_program() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "topos".to_owned())
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
    /// Honest per-bundle / per-surface FAILURE lines (the sweep's warning channel — the one that
    /// makes a run exit non-zero).
    pub warnings: Vec<Message>,
    /// Facts about work that SUCCEEDED and is worth stating (a wholly-owned config file deleted
    /// when its last entry left). A line describing something that worked belongs here: routed
    /// into the warning channel it would make a clean run report itself broken.
    pub notices: Vec<Message>,
    /// STANDING NOT-PLACED facts — a capability this build or this machine does not have, and an
    /// entry topos does not own already sitting where a placement would go. They print their line
    /// on every run (the condition is re-decided from the files each time, so the line stops the
    /// moment the condition does), and they never fail the run.
    ///
    /// The rule they are the reason for: **a run fails only when an act it attempted failed.**
    /// Nothing was attempted here — no config file was written, none could be — so there is no
    /// fault to report. Routed into `warnings` these exited non-zero forever on any machine that
    /// merely has an agent topos cannot render this bundle for, which taught an agent watching the
    /// exit status that its converging machine was broken.
    pub advisories: Vec<Message>,
    /// The BUNDLE IDENTITIES this converge could not place — a `server.json` the gate refused, a
    /// document that would not parse. Each already has a line in `warnings`, but a line is not a
    /// bundle: the sweep counts BUNDLES, and a gate failure that rode only the line channel exited
    /// non-zero under a summary saying "1 already up to date". The caller folds these into the
    /// count so the summary and the status describe the same run.
    ///
    /// The IDENTITY, never the display name — the same key [`BundleStates::bundle_id`] answers on,
    /// so the caller can join a failure onto the receipt row it belongs to. A workspace `linear`
    /// and a local `linear` can stand in one scope, and a name would make them one bundle: one
    /// tally entry for two failures, and one bundle's failure standing the other's row down.
    pub failed_bundles: Vec<String>,
    /// The BUNDLE IDENTITIES that reached NO surface for a STANDING reason — every agent's
    /// placement withheld by a capability gap, or blocked by an entry topos does not own. Keyed
    /// exactly like [`Self::failed_bundles`], and counted in the summary's own clause (`not
    /// placed`) rather than under `failed`: nothing broke, and nothing arrived either, so the two
    /// facts get one word each instead of the wrong one twice.
    pub unplaced_bundles: Vec<String>,
}

// =================================================================================================
// The refusal a gated document earns.
// =================================================================================================

/// The ONE refusal line an unplaceable server document earns. The remedy names the audience that
/// can actually act: a local folder row is the reader's own file to correct; a workspace bundle is
/// an owner's.
fn gate_refusal(code: &str, name: &str, reason: &str, from_workspace: bool) -> Message {
    let remedy = if from_workspace {
        "ask a workspace owner to correct it, then run 'topos update'"
    } else {
        "correct its server.json, then run 'topos update'"
    };
    crate::message::failure(
        code,
        format!("{name}: {reason}. Nothing is placed for it — {remedy}."),
    )
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
    picked: &BTreeSet<String>,
    hold: &HashSet<String>,
    allow_removals: bool,
) -> ConvergeOutcome {
    let mut out = ConvergeOutcome::default();
    // The one place a bundle id becomes the word a person reads. Built once, from the demands
    // this run planned — the only names the converge is ever given.
    let names: BTreeMap<String, String> = demands
        .iter()
        .map(|d| (d.bundle_id.clone(), d.name.clone()))
        .collect();
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
            out.warnings.push(crate::message::failure(
                "MCP_CUSTODY_UNREADABLE",
                format!(
                    "{}: topos's record of which MCP config entries it owns could not be read \
                     ({}), so no MCP config file was read or written this run. Without that \
                     record topos cannot tell its own entries from yours, and it will not guess. \
                     Inspect that file by hand.",
                    io.layout.config_custody_path().display(),
                    e.detail()
                ),
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
            out.warnings.push(crate::message::failure(
                "MCP_CUSTODY_WRITE_FAILED",
                "topos could not record the repair of its own MCP entry list, so it skipped MCP \
                 config files this run. Run 'topos update' to try again."
                    .to_owned(),
            ));
            return out;
        }
    }

    // The bundles this scope had ALREADY placed when the converge began — read off the durable
    // records after recovery, so it is the record and not a guess. A bundle absent here whose
    // entry this run writes is being placed for the FIRST time in this scope (see
    // [`BundleStates::first_placement`]).
    let placed_before: HashSet<String> = custody.placed_bundles().into_iter().collect();

    // Parse every demand once — the PLACEMENT parse ([`crate::mcp_render`]), which is what says
    // whether these bytes can become an entry at all and which fails closed on the shapes a
    // placed entry must never carry (a secret / templated / value-less header). Nothing here
    // re-decides what may be SHARED: a workspace's servers are validated where they are written
    // down, and a local row's document is the person's own file. A parse failure HOLDS the bundle
    // (its standing entries must not read as undemanded, nothing new is placed).
    let mut parsed: Vec<(usize, ServerDoc)> = Vec::new();
    let mut failed: BTreeMap<usize, String> = BTreeMap::new();
    for (i, d) in demands.iter().enumerate() {
        match crate::mcp_render::parse_server_json(&d.server_json) {
            Ok(p) => parsed.push((i, p)),
            Err(reason) => {
                out.warnings.push(gate_refusal(
                    "MCP_UNPLACEABLE",
                    &d.name,
                    &reason,
                    d.workspace_slug.is_some(),
                ));
                // The bundle could not be carried forward — so it is counted as one. The line
                // alone left the sweep exiting non-zero under a summary that named no failure.
                // Filed under the IDENTITY the demand was planned with: the name is what the
                // warning line above says, and nothing else.
                if !out.failed_bundles.contains(&d.bundle_id) {
                    out.failed_bundles.push(d.bundle_id.clone());
                }
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

    // What a reservation is measured against, read ONCE and only when one stands: which keys this
    // scope's harnesses are observed to hold, right now, before this run writes anything. A
    // retired name goes back only to a mint for the same server with nothing left under the key
    // (see `ConfigCustody::mint_key`), so the evidence is gathered before the first mint and not
    // after the last removal.
    let standing = if custody.doc.retired.is_empty() {
        None
    } else {
        standing_keys(io, descriptors)
    };
    // Mint keys for the placeable demands (durable with the first write below).
    let minted: BTreeMap<usize, String> = parsed
        .iter()
        .map(|(i, doc)| {
            let d = &demands[*i];
            // The server this DOCUMENT names — the address it offers, else the package it pins.
            // A document-level fact deliberately: the same bundle mints the same key whichever
            // form each agent here ends up running.
            let address = doc.canonical_identity();
            let key = custody.mint_key(
                &d.bundle_id,
                &d.name,
                d.workspace_slug.as_deref(),
                &config_custody::KeyMint {
                    address: address.as_deref(),
                    standing: standing.as_ref(),
                },
            );
            (*i, key)
        })
        .collect();

    // Per-bundle state collector, keyed by demand index (order preserved for the receipt).
    let mut states: BTreeMap<usize, Vec<McpAgentState>> = BTreeMap::new();
    // The demands whose config entries this run WROTE somewhere — see [`BundleStates::wrote`].
    let mut wrote: BTreeSet<usize> = BTreeSet::new();
    // The demands SOME engaged agent could not be given anything for (a capability this machine or
    // this build does not have). Tracked apart from the states, because `Withheld` also names the
    // ordinary structural narrowings — a scope with no surface, a `dest` that excludes an agent —
    // and those are not a bundle failing to arrive.
    let mut capability_gaps: BTreeSet<usize> = BTreeSet::new();
    // The STANDING not-placed lines, collected rather than pushed: which bundle a cause belongs to
    // is only answerable once every harness has answered, and a line two bundles share has to name
    // them or it attributes one bundle's gap to the other. `(code, sentence) → the demands`, in
    // first-seen order, so the emitted order is still the descriptor order the receipt reads in.
    let mut standing: Vec<(&'static str, String, BTreeSet<usize>)> = Vec::new();
    let mut note_standing = |code: &'static str, text: String, demand: usize| match standing
        .iter_mut()
        .find(|(c, t, _)| *c == code && *t == text)
    {
        Some((_, _, who)) => {
            who.insert(demand);
        }
        None => standing.push((code, text, BTreeSet::from([demand]))),
    };

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
                        let line = crate::placement::escape_message(h.slug, &path);
                        if !out.warnings.contains(&line) {
                            out.warnings.push(line);
                        }
                    }
                    continue;
                };
                // Engagement: the harness is picked on this machine, OR its config surface
                // already exists (entries were placed while it was picked — removal must still
                // reach them; the plan admits no new entry for an unpicked agent).
                if !(picked.contains(h.slug) || io.fs.exists(&root)) {
                    continue;
                }
                (file, dialect)
            }
        };

        // The desired set for this harness: the placeable demands whose PLAN puts an entries
        // target here. Nothing is re-narrowed — the plan already answered.
        //
        // WHAT each of them becomes is decided per harness, here and nowhere else
        // ([`crate::mcp_render::select`]): the address this agent dials, the bridge that gets the
        // address to an agent that dials nothing, or the package it runs. A bundle this MACHINE
        // cannot set up for this agent — a registry with no runtime arm, a runtime that is not
        // installed, a value only a person can fill in — is WITHHELD with the reason said in
        // plain words, and re-decided from scratch on the next sweep.
        let caps = h.mcp().map(|m| crate::mcp_render::HarnessCaps {
            // A ROW may not promise a shape its DIALECT has no grammar for — in EITHER direction.
            // The capability and the dialect are separate columns, and a table that set them at
            // odds (a downloaded one, an override) would have the driver refuse the whole surface,
            // taking every OTHER bundle's entry in that file down with it. Answering here
            // withholds one placement instead, which is what a capability gap is supposed to cost.
            // Both questions are asked of an EMPTY target, because the dialect answers on the
            // shape and never on the contents.
            remote: m.remote && mcp::dialect_expresses(dialect, &EMPTY_ADDRESS),
            stdio: m.stdio && mcp::dialect_expresses(dialect, &EMPTY_PROGRAM),
            env_ref: m.env_ref,
        });
        let machine = crate::mcp_render::Machine::at(&file);
        let mut desired: Vec<McpEntry> = Vec::new();
        let mut desired_bundles: BTreeMap<String, usize> = BTreeMap::new();
        for (i, doc) in &parsed {
            if demands[*i].plan.entries_for(h.slug).is_none() {
                continue;
            }
            let Some(caps) = caps else { continue };
            let gateway = demands[*i].gateway_bearer();
            let target = match crate::mcp_render::select(
                doc,
                caps,
                topos_harness::registry::mcp_bridge(),
                Some(&io.relay_program),
                io.runtimes,
                machine,
                gateway,
            ) {
                Ok(target) => target,
                Err(gap) => {
                    push_state(
                        &mut states,
                        *i,
                        agent_state(
                            h.slug,
                            TargetOutcome::Withheld,
                            Some(&gap.note(h.slug)),
                            None,
                        ),
                    );
                    note_standing(gap.code(), gap.message(h.slug), *i);
                    capability_gaps.insert(*i);
                    continue;
                }
            };
            let key = minted[i].clone();
            desired_bundles.insert(key.clone(), *i);
            desired.push(McpEntry {
                key,
                target,
                // The publisher's word, EXCEPT where the workspace reaches the server itself: that
                // entry is dialed with the credential the target already carries, so there is no
                // sign-in for a harness to start and no hint to give it.
                auth: doc.entry_auth(gateway),
            });
        }
        // THE COLLISION PRE-FLIGHT — asked before the drivers, because the drivers can only see
        // entries that LOOK like topos's. A server the agent already has under somebody else's
        // name, here or in a file this harness also reads, is not something to write over or
        // beside: the placement is skipped whole and the reason is said with the way out.
        let blocked = collisions(io, &custody, h, &file, dialect, &desired);
        for (key, hit) in &blocked {
            if let Some(i) = desired_bundles.get(key) {
                push_state(
                    &mut states,
                    *i,
                    agent_state(
                        h.slug,
                        TargetOutcome::Conflicting,
                        Some(&hit.note()),
                        Some(&hit.path),
                    ),
                );
                let (code, text) = hit.message(h.slug, &io.home);
                note_standing(code, text, *i);
            }
        }
        if !blocked.is_empty() {
            desired.retain(|e| !blocked.contains_key(&e.key));
            desired_bundles.retain(|key, _| !blocked.contains_key(key));
        }

        // Parse-failed demands report per engaged harness (their entries are held above).
        for (i, reason) in &failed {
            if demands[*i].plan.entries_for(h.slug).is_some() {
                push_state(
                    &mut states,
                    *i,
                    agent_state(
                        h.slug,
                        TargetOutcome::Unprovable,
                        Some(reason.as_str()),
                        None,
                    ),
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
            &names,
        );
        out.warnings.extend(surface_out.warnings);
        out.notices.extend(surface_out.notices);
        for key in &surface_out.wrote {
            if let Some(i) = desired_bundles.get(key) {
                wrote.insert(*i);
            }
        }
        // WHAT ELSE DIALS THIS SERVER, asked of the entries topos already HOLDS. The pre-flight
        // above deliberately never blocks a key topos owns here — dropping it would uninstall a
        // placement that stands — but "never block it" is not "never mention it": a foreign entry
        // for the same server in a file this agent reads FIRST makes topos's own entry dead
        // config, and the machine looked converged while the agent used somebody else's copy.
        // Re-read from the files every run and stored nowhere, so it clears itself the moment the
        // other entry goes.
        let shadowed = shadows(io, h, &desired, &blocked);
        for (key, state) in surface_out.states {
            let mut state = state;
            if state.state == TargetOutcome::Current
                && let Some(note) = shadowed.get(&key)
            {
                state.note = Some(note.clone());
            }
            match desired_bundles.get(&key) {
                Some(i) => push_state(&mut states, *i, state),
                None => {
                    // A key outside the desired set: a removal (or a drifted survivor of one),
                    // reported with its bundle.
                    if matches!(state.state, TargetOutcome::Removed | TargetOutcome::Drifted)
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

    // THE STANDING LINES, once each, in descriptor order. A cause TWO bundles share is said once
    // per bundle with the bundle named: the sentence itself carries no name, so a bare dedup
    // printed one machine's missing node over both bundles and attributed it to neither.
    for (code, text, who) in standing {
        let attributed = who.len() > 1;
        for i in who {
            let text = if attributed {
                format!("{}: {text}", demands[i].name)
            } else {
                text.clone()
            };
            let line = crate::message::advisory(code, text);
            if !out.advisories.contains(&line) {
                out.advisories.push(line);
            }
        }
    }

    // A bundle every surface REFUSED is not an annotated success: nothing of it is installed
    // anywhere. It is not a FAILURE either — nothing was attempted, so nothing failed — so it
    // takes the summary's own `not placed` clause, which is the one word that is true about it.
    // Counting it under `failed` (and exiting non-zero on it, forever) called a machine that
    // merely lacks node broken; leaving it out of the count entirely printed "1 already up to
    // date" over a machine holding nothing. A bundle blocked on ONE surface and placed on another
    // is neither: it is installed, and its receipt row carries the collision beside the placements.
    for (i, d) in demands.iter().enumerate() {
        let blocked = capability_gaps.contains(&i)
            || states
                .get(&i)
                .is_some_and(|s| s.iter().any(|st| st.state == TargetOutcome::Conflicting));
        if blocked
            && !wrote.contains(&i)
            && !custody.has_entries_for(&d.bundle_id)
            && !out.failed_bundles.contains(&d.bundle_id)
            && !out.unplaced_bundles.contains(&d.bundle_id)
        {
            out.unplaced_bundles.push(d.bundle_id.clone());
        }
    }

    // Retirement: a bundle with no remaining entries anywhere, not demanded and not held, gives
    // its key back to the reserve. It STAYS reserved from here — a name goes back only to a mint
    // that proves it is for the same server, which is a question only a later mint can ask.
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
    picked: &BTreeSet<String>,
    bundle_id: &str,
    /* the name a person calls it — see `converge`'s `names` */ name: &str,
) -> ConvergeOutcome {
    let mut out = ConvergeOutcome::default();
    let names: BTreeMap<String, String> =
        std::iter::once((bundle_id.to_owned(), name.to_owned())).collect();
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
            out.warnings.push(crate::message::failure(
                "MCP_CUSTODY_UNREADABLE",
                format!(
                    "{}: topos's record of which MCP config entries it owns could not be read \
                     ({}), so the bundle's MCP entries are left in place. Inspect that file by \
                     hand.",
                    io.layout.config_custody_path().display(),
                    e.detail()
                ),
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
            out.warnings.push(crate::message::failure(
                "MCP_CUSTODY_WRITE_FAILED",
                "topos could not record the repair of its own MCP entry list, so the bundle's MCP \
                 entries are left in place. Run 'topos update', then remove it again."
                    .to_owned(),
            ));
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
        if !(picked.contains(h.slug) || io.fs.exists(&root)) {
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
            &names,
        );
        out.warnings.extend(surface_out.warnings);
        out.notices.extend(surface_out.notices);
        for (state_key, state) in surface_out.states {
            if state_key == key
                && matches!(state.state, TargetOutcome::Removed | TargetOutcome::Drifted)
            {
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

// =================================================================================================
// The collision pre-flight — what already stands where a placement would go.
// =================================================================================================

/// One entry standing in the way of a placement: the name it goes by, the file it is in, and
/// whether it LOOKS like something an older topos left (which is a different sentence, because
/// nothing here can prove it).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Collision {
    name: String,
    path: PathBuf,
    /// WHICH identity matched. A name a person can see is taken and an address hiding under
    /// somebody else's name are different discoveries, and a line that called the second one a
    /// name clash sent a reader looking for a name that was never there.
    by: CollisionBy,
    /// A `topos-`-prefixed name with no record behind it, IN A FILE TOPOS WRITES. Named as a
    /// POSSIBILITY: the prefix is a spelling, not a provenance, and topos does not claim what it
    /// cannot prove. Never claimed in a file topos only reads — anyone may name an entry anything
    /// there, and calling a stranger's entry topos's leftover is a guess about somebody else's
    /// file.
    possible_leftover: bool,
}

/// The two ways an entry topos does not own blocks a placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CollisionBy {
    /// It holds the very key topos would write.
    Name,
    /// It points at the same server under a different name.
    Address,
}

/// A path as a person reads it — this scope's home abbreviated to `~`, the spelling every other
/// receipt line uses. One receipt printing `~/.cursor/mcp.json` on one line and
/// `/Users/somebody/.claude.json` on the next reads as two different machines.
///
/// The client's own `inventory::pretty` does this from a `Ctx`; the converge holds only its
/// `ScopeIo`, and the rule is one line long, so it is spelled here against the home the converge
/// already resolved rather than by threading a context through the engine.
fn tilde(home: &Path, path: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

impl Collision {
    /// The short cause the per-agent state carries (the receipt prints it after the outcome).
    fn note(&self) -> String {
        if self.possible_leftover {
            return "possibly left by an earlier topos version and no longer managed".to_owned();
        }
        match self.by {
            CollisionBy::Name => format!(
                "an entry named \"{}\" is already here and topos does not manage it",
                self.name
            ),
            CollisionBy::Address => format!(
                "already dials this server's address as \"{}\", and topos does not manage it",
                self.name
            ),
        }
    }

    /// The whole sentence, with the way out. A person's next move is to delete an entry topos will
    /// never delete for them, so the line names WHICH entry, in WHICH file, and what happens then.
    fn message(&self, slug: &str, home: &Path) -> (&'static str, String) {
        let path = tilde(home, &self.path);
        let name = &self.name;
        if self.possible_leftover {
            return (
                "MCP_ENTRY_LEFTOVER",
                format!(
                    "possible leftover from an earlier topos version: {name} in {path} is no \
                     longer managed. Remove it by deleting the \"{name}\" entry from that file."
                ),
            );
        }
        match self.by {
            CollisionBy::Name => (
                "MCP_ENTRY_CONFLICT",
                format!(
                    "not placed in {slug}: an entry for this server already exists ({name} in \
                     {path}) and topos does not manage it. Remove it to let topos manage this \
                     server, then run 'topos update'."
                ),
            ),
            CollisionBy::Address => (
                "MCP_ENTRY_CONFLICT",
                format!(
                    "not placed in {slug}: an entry topos does not manage ({name} in {path}) \
                     already dials this server's address. Remove it to let topos manage this \
                     server, then run 'topos update'."
                ),
            ),
        }
    }
}

/// **What already stands where each desired entry would go** — by NAME or by the SERVER it points
/// at, in this surface and in every file the harness also reads servers from ([`KnownHarness`]'s
/// read-only conflict paths). Answers the desired keys that must not be placed, each with the
/// entry blocking it.
///
/// Three rules make this safe rather than merely careful:
///
/// - **An entry topos owns here is never a collision.** A row proves topos wrote it; a second
///   bundle pointing at the same server is the existing conflicting-key story, unchanged.
/// - **A key topos already holds on this surface is never blocked.** Its entry is already there,
///   and dropping it from the desired set would take it back out through the drivers' own removal
///   — a foreign duplicate appearing later must never uninstall a placement that stands.
/// - **A surface that will not read blocks nothing.** Absence is unprovable there, and a collision
///   nobody can see is a duplicate at worst; refusing on a guess would strand a delivery.
fn collisions(
    io: &ScopeIo<'_>,
    custody: &ScopeEntries<'_>,
    h: &KnownHarness,
    file: &Path,
    dialect: McpDialect,
    desired: &[McpEntry],
) -> BTreeMap<String, Collision> {
    let mut out = BTreeMap::new();
    if desired.is_empty() {
        return out;
    }
    // The keys topos's own record puts in THIS file — both the entries that are not collisions and
    // the placements that must not be blocked.
    let ours_here: BTreeSet<String> = custody
        .rows_at(h.slug, file)
        .into_iter()
        .map(|(_, e)| e.key)
        .collect();
    // Each desired entry's server, canonicalized once.
    let wanted: Vec<(&str, Option<String>)> = desired
        .iter()
        .filter(|e| !ours_here.contains(&e.key))
        .map(|e| (e.key.as_str(), e.address()))
        .collect();
    if wanted.is_empty() {
        return out; // every desired entry is already topos's here
    }
    // The dest file first (its entries may be topos's), then the read-only files, in table order.
    let mut surfaces: Vec<(PathBuf, McpDialect, Option<&str>, bool)> =
        vec![(file.to_path_buf(), dialect, None, true)];
    surfaces.extend(
        h.mcp_conflict_paths(&io.home)
            .into_iter()
            .map(|(path, dialect, selector)| (path, dialect, selector, false)),
    );
    for (path, dialect, selector, may_be_ours) in surfaces {
        let Ok(Some(bytes)) = io.fs.read_opt(&path) else {
            continue; // absent, or unreadable — nothing is provable there
        };
        let Some(seen) = mcp::observe_entries(dialect, Some(&bytes), selector) else {
            continue;
        };
        for entry in seen {
            if may_be_ours && ours_here.contains(&entry.name) {
                continue; // topos's own entry, this bundle's or another's
            }
            let address = entry.address.as_deref();
            for (key, wanted_address) in &wanted {
                if out.contains_key(*key) {
                    continue;
                }
                let same_name = entry.name == *key;
                let same_server = address.is_some() && address == wanted_address.as_deref();
                if same_name || same_server {
                    out.insert(
                        (*key).to_owned(),
                        Collision {
                            name: entry.name.clone(),
                            path: path.clone(),
                            by: if same_name {
                                CollisionBy::Name
                            } else {
                                CollisionBy::Address
                            },
                            // Only where topos itself writes: a `topos-` name in a file topos
                            // merely READS is somebody else's entry with a familiar-looking name,
                            // and the leftover wording would claim a provenance nobody can prove.
                            possible_leftover: may_be_ours && entry.name.starts_with("topos-"),
                        },
                    );
                }
            }
        }
    }
    out
}

/// **What ELSE dials the same server, in a file this harness reads BEFORE the one topos writes.**
///
/// The collision pre-flight answers "may this entry be written"; this answers the question that
/// only matters once it HAS been — whether the agent will ever read it. A harness's
/// [`KnownHarness::mcp_conflict_paths`] are read-only surfaces it consults itself, and an entry
/// there for the same server wins: topos's entry stands, byte-perfect and recorded, and the agent
/// uses the other one. Reporting nothing left a machine looking fully converged while every call
/// went through somebody else's copy.
///
/// It is a NOTE and never a refusal: nothing is wrong with either entry, the person may well have
/// meant it, and topos deletes neither. It is re-decided from the files on every run and stored
/// nowhere, so removing the other entry clears it on the next sweep with no state to reconcile.
///
/// Keys already BLOCKED by the pre-flight are skipped — they have a louder line of their own —
/// and so is any surface that will not read, exactly as the pre-flight does: an unprovable
/// absence is not evidence of one.
fn shadows(
    io: &ScopeIo<'_>,
    h: &KnownHarness,
    desired: &[McpEntry],
    blocked: &BTreeMap<String, Collision>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let wanted: Vec<(&str, String)> = desired
        .iter()
        .filter(|e| !blocked.contains_key(&e.key))
        .filter_map(|e| e.address().map(|a| (e.key.as_str(), a)))
        .collect();
    if wanted.is_empty() {
        return out;
    }
    for (path, dialect, selector) in h.mcp_conflict_paths(&io.home) {
        let Ok(Some(bytes)) = io.fs.read_opt(&path) else {
            continue;
        };
        let Some(seen) = mcp::observe_entries(dialect, Some(&bytes), selector) else {
            continue;
        };
        for entry in seen {
            for (key, address) in &wanted {
                if out.contains_key(*key) || entry.name == *key {
                    continue; // the same key in both files is one entry's story, not two
                }
                if entry.address.as_deref() == Some(address.as_str()) {
                    out.insert(
                        (*key).to_owned(),
                        format!(
                            "also configured as \"{}\" in {}, which this agent prefers",
                            entry.name,
                            tilde(&io.home, &path)
                        ),
                    );
                }
            }
        }
    }
    out
}

/// **Every entry key this scope's harnesses are OBSERVED to hold** — half the evidence a name
/// reservation is measured against ([`config_custody::ConfigCustody::mint_key`]; the other half is
/// the server address). It reads EVERY file a harness reads servers from: the surface topos writes
/// AND the read-only [`KnownHarness::mcp_conflict_paths`] the collision pre-flight already asks. A
/// key still standing in one of those is exactly as inheritable as one in the writable surface —
/// leaving them out let a reservation go back while an entry stood under it in Claude's own
/// `~/.claude.json`.
///
/// It answers `None` the moment ONE of those files cannot be read or parsed: absence is then
/// unprovable, and a reservation dropped on an unreadable file is a name handed to a new bundle
/// over an entry that may still be standing under it.
///
/// It sees keys, not ownership: a drifted entry, an unrecorded one, a foreign one and a leftover
/// all count, because all four are what a re-minted name would inherit.
fn standing_keys(
    io: &ScopeIo<'_>,
    descriptors: &[&'static KnownHarness],
) -> Option<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    for h in descriptors {
        if let crate::placement::ConfigSurface::Ready { file, dialect, .. } =
            crate::placement::config_surface(h, &io.home, io.project_root.as_deref())
        {
            let Ok(current) = io.fs.read_opt(&file) else {
                return None; // unreadable: nothing about this scope's names is provable
            };
            // No file, no entries — the one honest absence.
            if let Some(bytes) = current {
                let observed = mcp::observe(dialect, Some(&bytes));
                if !observed.parseable {
                    return None;
                }
                keys.extend(observed.entries.into_keys());
            }
        }
        for (path, dialect, selector) in h.mcp_conflict_paths(&io.home) {
            let Ok(current) = io.fs.read_opt(&path) else {
                return None;
            };
            let Some(bytes) = current else {
                continue;
            };
            let Some(seen) = mcp::observe_entries(dialect, Some(&bytes), selector) else {
                return None; // unparseable: the same unprovable absence
            };
            keys.extend(seen.into_iter().map(|e| e.name));
        }
    }
    Some(keys)
}

/// Move a bundle's SURVIVING config rows out of its record, under the converge lock, because the
/// record is about to be deleted (see [`crate::config_custody::detach_to_unrecorded`]). Returns the
/// caller's warning lines — best-effort, because the removal itself already landed.
pub(crate) fn detach_bundle_rows(io: &ScopeIo<'_>, bundle_id: &str) -> Vec<Message> {
    let _lock = match converge_lock(io) {
        Ok(guard) => guard,
        Err(warning) => return vec![warning],
    };
    match config_custody::detach_to_unrecorded(io.fs, io.layout, bundle_id) {
        Ok(_) => Vec::new(),
        Err(e) => vec![crate::message::failure(
            "MCP_CUSTODY_WRITE_FAILED",
            format!(
                "{bundle_id}: topos could not update its own record of MCP entries ({}). An entry \
                 left standing in your MCP config is no longer tracked — remove it by hand if you \
                 do not want it.",
                e.detail()
            ),
        )],
    }
}

/// The per-scope MCP converge lock (`locks/mcp.lock`, blocking): every entry point that runs the
/// custody + config read-modify-write — the sweep's [`converge`] (which a targeted `update` and
/// an `add` reach through the same narrowed reconcile) and [`remove_bundle`] — serializes on it,
/// so two processes can never interleave a read-modify-write over the same scope's configs.
///
/// LOCK ORDER, fixed: the sweep already holds `locks/currency.lock` when it converges, so this
/// lock is strictly INNER — taken only inside [`converge`]/[`remove_bundle`], released on return,
/// and NOTHING acquires another lock while holding it. No path takes it twice: the two holders
/// never call each other.
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
fn converge_lock(io: &ScopeIo<'_>) -> Result<crate::fs_seam::LockGuard, Message> {
    let locks = io.layout.locks_dir();
    // Created only when absent so the common case adds no mutating op (the crash sweep counts
    // them).
    if !io.fs.exists(&locks)
        && let Err(e) = io.fs.create_dir_all(&locks)
    {
        return Err(crate::message::failure(
            "MCP_LOCK_UNAVAILABLE",
            format!(
                "{}: topos could not create this folder ({e}), so no MCP config file was read or \
                 written this run. Run 'topos update' to try again.",
                locks.display()
            ),
        ));
    }
    io.fs.lock_exclusive(&locks.join("mcp.lock")).map_err(|e| {
        crate::message::failure(
            "MCP_LOCK_UNAVAILABLE",
            format!(
                "topos could not take its MCP lock ({e}) — another topos may be running — so no \
                 MCP config file was read or written this run. Run 'topos update' to try again."
            ),
        )
    })
}

/// A program-run target with nothing in it — the probe that asks a DIALECT whether it has any
/// grammar for one at all ([`mcp::dialect_expresses`] answers on the shape, never on the contents).
static EMPTY_PROGRAM: mcp::McpTarget = mcp::McpTarget::Local {
    command: String::new(),
    args: Vec::new(),
    env: Vec::new(),
    env_ref: topos_harness::mcp::descriptor::EnvRef::DollarBrace,
};

/// The same probe for the other shape — a dialed ADDRESS with nothing in it. A config file whose
/// harness reaches remote servers somewhere other than the file (Claude Desktop adds them in the
/// app) has no address key to write, and says so here rather than at the driver.
static EMPTY_ADDRESS: mcp::McpTarget = mcp::McpTarget::Remote {
    url: String::new(),
    headers: Vec::new(),
};

fn agent_state(
    slug: &str,
    state: TargetOutcome,
    note: Option<&str>,
    file: Option<&Path>,
) -> McpAgentState {
    McpAgentState {
        agent: slug.to_owned(),
        state,
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
/// the keys whose bytes this run actually WROTE into the config, and the surface's warnings.
/// Custody movement is not reported here — the rows live on [`ScopeEntries`], which tracks its own
/// dirty set and persists it in one `flush`.
struct SurfaceOutcome {
    states: Vec<(String, McpAgentState)>,
    /// The keys this surface WROTE this run — placed, updated, or repaired. Empty on a surface
    /// left byte-identical, which is what makes it an answer rather than a guess.
    wrote: BTreeSet<String>,
    warnings: Vec<Message>,
    /// Success NOTICES — things that WORKED and are worth stating (see [`ConvergeOutcome::notices`]).
    notices: Vec<Message>,
}

impl SurfaceOutcome {
    fn empty() -> Self {
        Self {
            states: Vec::new(),
            wrote: BTreeSet::new(),
            warnings: Vec::new(),
            notices: Vec::new(),
        }
    }

    /// The WRITE-BOUNDARY containment refusal: every desired key reads `unprovable` with the same
    /// note the planner's withheld line carries, nothing is written, and the escape is disclosed.
    fn escaped(desired: &[McpEntry], h: &KnownHarness, escape: &Path) -> Self {
        Self {
            states: desired
                .iter()
                .map(|e| {
                    (
                        e.key.clone(),
                        agent_state(
                            h.slug,
                            TargetOutcome::Unprovable,
                            Some("the config path does not resolve inside this checkout"),
                            None,
                        ),
                    )
                })
                .collect(),
            wrote: BTreeSet::new(),
            warnings: vec![crate::placement::escape_message(h.slug, escape)],
            notices: Vec::new(),
        }
    }
    fn unprovable(desired: &[McpEntry], h: &KnownHarness, path: &Path, reason: &str) -> Self {
        Self {
            states: desired
                .iter()
                .map(|e| {
                    (
                        e.key.clone(),
                        agent_state(h.slug, TargetOutcome::Unprovable, Some(reason), Some(path)),
                    )
                })
                .collect(),
            wrote: BTreeSet::new(),
            warnings: vec![crate::message::failure(
                "MCP_SURFACE_UNPROVABLE",
                format!(
                    "{}'s MCP config: {reason}. topos changed nothing in that file.",
                    h.slug
                ),
            )],
            notices: Vec::new(),
        }
    }

    /// topos's OWN ownership record could not be saved for this surface, so the config file was
    /// never opened. The fault is the record; the consequence is this file — and the line says
    /// both, in that order, because "the ledger" is not something a person has a folder for while
    /// the config file is. The short CAUSE rides each state's note (a receipt puts it after a
    /// dash); the whole sentence, keyed by the file, rides the machine channel.
    fn ledger_unwritable(
        desired: &[McpEntry],
        h: &KnownHarness,
        path: &Path,
        detail: &str,
    ) -> Self {
        let cause = format!("topos could not save its record of the entries it owns ({detail})");
        Self {
            states: desired
                .iter()
                .map(|e| {
                    (
                        e.key.clone(),
                        agent_state(h.slug, TargetOutcome::Unprovable, Some(&cause), Some(path)),
                    )
                })
                .collect(),
            wrote: BTreeSet::new(),
            warnings: vec![crate::message::failure(
                "MCP_CUSTODY_WRITE_FAILED",
                format!(
                    "{}: topos could not save its record of the entries it owns there ({detail}), \
                     so it wrote nothing to that file.",
                    path.display()
                ),
            )],
            notices: Vec::new(),
        }
    }
}

/// Why one surface's journaled write did not land. The LEDGER arm is kept apart because it is the
/// one fault whose cause is topos's own bookkeeping rather than the person's config — and a line
/// that says "this file could not be read" about a file topos read perfectly well sends someone
/// to fix the wrong thing.
enum WriteFault {
    Ledger(String),
    Other(String),
}

/// **The containment rail, re-run at the WRITE boundary.** A project surface was proven inside the
/// checkout when the plan was made — and a plan is a memory, not a permission. Any component of
/// the path can be swapped for an outward symlink between planning and this write, and
/// `replace_config` follows symlinks, so the proof is re-run HERE, immediately before any byte
/// moves, over every path this surface is about to touch (the config file, and the plugin manifest
/// beside it). `None` at the person scope — there is no checkout to be inside of — and when every
/// path still proves.
fn write_escape(io: &ScopeIo<'_>, paths: &[&Path]) -> Option<PathBuf> {
    let root = io.project_root.as_deref()?;
    paths
        .iter()
        .find(|p| !crate::placement::within_project(root, p))
        .map(|p| (*p).to_path_buf())
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
) -> Result<(), WriteFault> {
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
        return Err(WriteFault::Other(format!(
            "an earlier config write is not finished yet, so topos skipped {} this run — the next \
             'topos update' finishes it",
            path.display()
        )));
    }
    if let Err(e) = custody.journal(io.fs, io.layout, intents) {
        custody.drop_journal(io.fs, io.layout);
        return Err(WriteFault::Ledger(e.detail()));
    }
    if let Err(e) = write() {
        // The config write FAILED — which is not the same as "nothing landed". A replace is
        // several syscalls, and an error after the rename leaves the new bytes in the file. So the
        // intents STAY journaled: the journal's whole purpose is to be resolved by OBSERVING the
        // file, and next run's recovery promotes what actually landed and drops what did not.
        // Clearing them here on the assumption that the error means "no bytes moved" is exactly
        // how a live entry ends up with no record — permanently unremovable, read as a hand edit
        // by every run afterwards. One extra recovery pass is the whole cost of not guessing.
        return Err(WriteFault::Other(format!(
            "writing {} failed ({e})",
            path.display()
        )));
    }
    // Promote: pending → each bundle's own rows, exactly as recovery would.
    custody.promote_journal();
    let failures = custody.flush(io.fs, io.layout);
    if !failures.is_empty() {
        // The rows moved in memory and the intents are still journaled ON DISK — the next run's
        // recovery promotes them again. Disclose, never lose.
        return Err(WriteFault::Other(format!(
            "{} — the next 'topos update' finishes it",
            failures
                .iter()
                .map(|f| f.text.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )));
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
    // `names`: bundle id → the name a person calls it, for the rows this run demands. A stale
    // row's bundle may not be demanded at all (that is what makes it stale), and then its id is
    // all there is.
    names: &BTreeMap<String, String>,
) -> SurfaceOutcome {
    let stale: Vec<Message> = custody
        .stale_rows(h.slug, path)
        .into_iter()
        .map(|(key, bundle, old)| {
            // The BUNDLE is what a person recognizes. `key` is the config key topos minted
            // (`topos-<ws>-<name>`, collision-suffixed) — an internal identity that this line
            // used to print as though it were the bundle's name. It is still named, because the
            // stale entry is findable by nothing else once you open the old file.
            crate::message::failure(
                "MCP_ENTRY_STALE_PATH",
                format!(
                    "'{}': topos recorded an entry for {} in {old} (named `{key}` there), but \
                     {}'s config here is {}. The old entry is left in place — delete it by hand \
                     if you no longer want it.",
                    names.get(&bundle).map_or(bundle.as_str(), String::as_str),
                    h.slug,
                    h.slug,
                    path.display()
                ),
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
            "the folder topos owns for this agent holds files topos did not write",
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
                // Healing the manifest is a WRITE — it passes the same write-boundary proof.
                && write_escape(io, &[&manifest]).is_none()
                && crate::config_io::replace_config(io.fs, &manifest, &plugin_dir::manifest_bytes())
                    .is_ok()
            {
                // ONE truth on both channels: the keys that count as written say `placed`, the
                // state every reader joins on — a row whose verb reports a write while its own
                // per-agent line reads `current` has told a person two things.
                for (key, state) in &mut surface.states {
                    if state.state == TargetOutcome::Current
                        && outcome.fingerprints.iter().any(|(k, _)| k == key)
                    {
                        state.state = TargetOutcome::Refreshed;
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
            // THE WRITE-BOUNDARY CONTAINMENT PROOF — before the journal, before a byte moves, so
            // a refusal costs nothing and leaves nothing behind.
            let mut to_write: Vec<&Path> = vec![path];
            if let Some(m) = &manifest {
                to_write.push(m);
            }
            if let Some(escape) = write_escape(io, &to_write) {
                return SurfaceOutcome::escaped(desired, h, &escape);
            }
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
                        // A SUCCESS notice, not a fault: the file went because the last entry
                        // topos owned left it, which is the removal working. Pushed into the
                        // warning channel it would make a clean sweep count itself failed.
                        //
                        // It names WHAT WENT. The plugin surface is a whole topos-owned DIRECTORY
                        // — the entries file, the manifest beside it, and the folder itself all
                        // leave together — and naming only the `.mcp.json` described a third of
                        // the deletion to a person who might well go looking for the rest.
                        let gone = match (is_plugin, manifest_kept.is_none()) {
                            (true, true) => path.parent().unwrap_or(path),
                            _ => path,
                        };
                        let what = if gone == path { "file" } else { "folder" };
                        surface.notices.push(crate::message::disclosure(
                            "MCP_FILE_REMOVED",
                            format!(
                                "{}: this {what} held only entries topos placed, so topos deleted \
                                 it with the last one.",
                                tilde(&io.home, gone)
                            ),
                        ));
                    }
                    if let Some(kept) = manifest_kept {
                        surface.warnings.push(kept);
                    }
                    surface
                }
                Err(WriteFault::Ledger(detail)) => {
                    SurfaceOutcome::ledger_unwritable(desired, h, path, &detail)
                }
                Err(WriteFault::Other(reason)) => {
                    SurfaceOutcome::unprovable(desired, h, path, &reason)
                }
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
    custody: &ScopeEntries<'_>,
    slug: &str,
    path: &Path,
    fingerprints: &[(String, String)],
    kept: &BTreeSet<String>,
    provenance: &BTreeMap<String, (String, String)>,
    owns_file: bool,
) -> BTreeMap<String, PendingIntent> {
    // The RESOLVED spelling, so a row written through a symlinked home is still recognized as
    // this surface's on the next run (see `config_custody::canonical_file`).
    let file = custody.canonical(path).display().to_string();
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
    custody: &mut ScopeEntries<'_>,
    slug: &str,
    path: &Path,
    fingerprints: &[(String, String)],
    kept: &BTreeSet<String>,
    provenance: &BTreeMap<String, (String, String)>,
    owns_file_default: bool,
) {
    // The RESOLVED spelling — see [`write_intents`].
    let file = custody.canonical(path).display().to_string();
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
        // A foreign key nobody demanded is not this bundle's business at all — no outcome, no row.
        if matches!(st, EntryState::Foreign) && !desired_keys.contains(key.as_str()) {
            continue;
        }
        // DERIVED, never chosen: the driver's per-key state projects onto the ONE drift
        // vocabulary, and that plus "did this run write it" is the outcome. Nothing here names a
        // word — the words live in `TargetOutcome`, where the dir side reads them too.
        let wrote = matches!(st, EntryState::PlacedNew | EntryState::Updated);
        let outcome = crate::placement::Drift::of_entry(*st).outcome(wrote);
        if wrote {
            out.wrote.insert(key.clone());
        }
        let note = match outcome {
            // How the change goes live, in the harness's own words.
            TargetOutcome::Created | TargetOutcome::Refreshed => h.mcp().map(|m| m.reload_note),
            TargetOutcome::Drifted => Some("hand-edited since topos wrote it — left in place"),
            TargetOutcome::Conflicting => {
                Some("the config key is held by an entry topos does not own")
            }
            _ => None,
        };
        out.states
            .push((key.clone(), agent_state(h.slug, outcome, note, Some(path))));
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
) -> (Option<PathBuf>, Option<Message>) {
    let Some(manifest) = plugin_manifest_path(mcp_path) else {
        return (None, None);
    };
    match fs.read_opt(&manifest) {
        Ok(None) => (Some(manifest), None),
        Ok(Some(bytes)) if bytes == plugin_dir::manifest_bytes() => (Some(manifest), None),
        _ => {
            let kept = crate::message::failure(
                "MCP_PLUGIN_MANIFEST_KEPT",
                format!(
                    "{}: this is not the file topos wrote for {slug}, so topos left it alone.",
                    manifest.display()
                ),
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
fn prune_plugin_dir(fs: &dyn FsOps, slug: &str, mcp_path: &Path) -> Option<Message> {
    let dir = mcp_path.parent()?;
    let manifest = dir.join(plugin_dir::PLUGIN_MANIFEST_PATH);
    let mut kept = None;
    match fs.read_opt(&manifest) {
        Ok(None) => {}
        Ok(Some(bytes)) if bytes == plugin_dir::manifest_bytes() => {
            let _ = fs.remove_file(&manifest);
        }
        _ => {
            kept = Some(crate::message::failure(
                "MCP_PLUGIN_MANIFEST_KEPT",
                format!(
                    "{}: this is not the file topos wrote for {slug}, so topos left it alone and \
                     kept the folder around it.",
                    manifest.display()
                ),
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

// =================================================================================================
// The whole-scope ledger read — what a TEARDOWN would reach, answered without writing a byte.
// =================================================================================================

/// The MCP config files this scope's ownership ledger records entries in, split the way a scrub
/// would treat them.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RecordedSurfaces {
    /// Files holding at least one ledger-recorded entry that still matches what topos wrote —
    /// the entries a scrub takes out.
    pub owned: Vec<String>,
    /// Files holding at least one ledger-recorded entry whose bytes no longer match the record —
    /// hand-edited, so a scrub leaves it exactly where it is.
    pub drifted: Vec<String>,
}

/// Read-only: which config files this scope's LEDGER records topos entries in, and which of those
/// hold an entry someone has since edited by hand.
///
/// It asks the converge's own ownership question — does what stands in the file still fingerprint
/// to what the ledger recorded? — through the same `observe` + fingerprint pair, and answers it
/// with no lock and no write. That is what lets `uninstall`'s PREVIEW promise exactly the files
/// its apply will touch, and name the ones it will leave alone.
///
/// An unreadable ledger answers nothing at all (the same fail-closed rule the converge takes: with
/// no record, topos cannot tell its own entries from yours and will not guess).
pub(crate) fn recorded_surfaces(
    io: &ScopeIo<'_>,
    descriptors: &[&'static KnownHarness],
) -> RecordedSurfaces {
    let mut out = RecordedSurfaces::default();
    let Ok(custody) = ScopeEntries::load(io.fs, io.layout) else {
        return out;
    };
    for h in descriptors {
        let crate::placement::ConfigSurface::Ready { file, dialect, .. } =
            crate::placement::config_surface(h, &io.home, io.project_root.as_deref())
        else {
            continue;
        };
        let recorded = custody.prior_for(h.slug, &file);
        if recorded.is_empty() {
            continue;
        }
        let Ok(current) = io.fs.read_opt(&file) else {
            continue;
        };
        let observed = mcp::observe(dialect, current.as_deref());
        if !observed.parseable {
            continue;
        }
        let path = file.to_string_lossy().into_owned();
        let mut owned = false;
        let mut drifted = false;
        for (key, fingerprint) in &recorded {
            match observed.entries.get(key) {
                Some(live) if live == fingerprint => owned = true,
                Some(_) => drifted = true,
                None => {} // already gone — nothing to remove and nothing to leave
            }
        }
        if owned {
            out.owned.push(path.clone());
        }
        if drifted {
            out.drifted.push(path);
        }
    }
    out.owned.sort();
    out.owned.dedup();
    out.drifted.sort();
    out.drifted.dedup();
    out
}

/// Every bundle this scope's ledger holds entries for — the teardown's removal set, in a stable
/// order. Empty when the ledger cannot be read (fail closed: no record, no removal).
pub(crate) fn recorded_bundles(io: &ScopeIo<'_>) -> Vec<String> {
    ScopeEntries::load(io.fs, io.layout)
        .map(|custody| custody.placed_bundles().into_iter().collect())
        .unwrap_or_default()
}

// =================================================================================================
// A connected server's RECORD — the delivered-state document every demand site reads.
// =================================================================================================

/// One connected server's record as the converge wants it: the identity, the document as BYTES
/// (one serialization, done here, so every reader renders the same run of characters), and the
/// two facts a receipt discloses about the revision it came from.
#[derive(Debug, Clone)]
pub(crate) struct RecordedServer {
    pub revision_id: String,
    pub document: Vec<u8>,
    pub pinned: bool,
    pub revoked: bool,
}

/// Read a connected server's record from THIS scope's store (`skills/<id>/server.json`) — the
/// document the last delivery left there. `Ok(None)` when this scope holds no record for it yet.
///
/// This is the ONE demand source for a workspace server: the sweep writes the record from a fresh
/// delivery and then reads it back here, and an offline run reads exactly the same file. There is
/// no second path, so a machine with no network converges from what it was last given rather than
/// from nothing.
pub(crate) fn recorded_server(
    ctx: &crate::ctx::Ctx<'_>,
    sid: &crate::id::SkillId,
) -> Result<Option<RecordedServer>, ClientError> {
    let sp = ctx.layout.published(sid);
    let Some(rec) =
        crate::doc::read_doc::<topos_types::persisted::McpServerRecord>(ctx.fs, &sp.server)?
    else {
        return Ok(None);
    };
    let document = serde_json::to_vec(&rec.document).map_err(|e| {
        ClientError::Corrupt(format!(
            "{}: its recorded server document could not be read back ({e})",
            rec.name
        ))
    })?;
    Ok(Some(RecordedServer {
        revision_id: rec.revision_id,
        document,
        pinned: rec.pinned,
        revoked: rec.revoked,
    }))
}

/// Write a connected server's record into this scope's store, building the record directory when
/// this scope meets the bundle for the first time. The whole delivery for the kind: there is no
/// version to fetch and nothing to materialize, so this one write IS the sync.
///
/// A NEW record is the ordinary never-received baseline — `lock.json` naming the bundle and no
/// files, `sync.json` at zero, an empty `map.json` — plus [`SkillPaths::server`]. Those three say
/// exactly the true thing about a connected server: it holds no files, so it has no file list to
/// pin, no bytes to have received, and no folder to place. There is no object store either, which
/// is why every file verb refuses the kind rather than opening one.
///
/// [`SkillPaths::server`]: crate::sidecar::SkillPaths::server
///
/// # Errors
/// The record could not be written, or the document is not JSON.
pub(crate) fn record_server(
    ctx: &crate::ctx::Ctx<'_>,
    sid: &crate::id::SkillId,
    name: &str,
    revision_id: &str,
    document: &[u8],
    pinned: bool,
    revoked: bool,
) -> Result<(), ClientError> {
    // A document that is not JSON came from the WIRE, not from this machine's own state — so it
    // says so with the wire's own error rather than telling a person their install is unreadable.
    let document: serde_json::Value = serde_json::from_slice(document).map_err(|e| {
        ClientError::WireInvalid(format!("{name}: its server document is not JSON ({e})"))
    })?;
    let record = topos_types::persisted::McpServerRecord {
        schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
        skill_id: sid.as_str().to_owned(),
        name: name.to_owned(),
        revision_id: revision_id.to_owned(),
        document,
        pinned,
        revoked,
    };
    let _guard = crate::sidecar::lock_skill(ctx.fs, &ctx.layout, sid)?;
    if ctx.fs.exists(&ctx.layout.skill_dir(sid)) {
        let sp = ctx.layout.published(sid);
        // A catalog rename travels: the lock is what every by-name resolution reads, so a record
        // whose server was renamed answers to the new name from the next delivery on.
        if let Some(mut lock) =
            crate::doc::read_doc::<topos_types::persisted::Lock>(ctx.fs, &sp.lock)?
            && lock.name != name
        {
            lock.name = name.to_owned();
            crate::doc::write_doc(ctx.fs, &sp.lock, &lock)?;
        }
        return crate::doc::write_doc(ctx.fs, &sp.server, &record);
    }
    let (staging_base, sp) = ctx.layout.staging(sid);
    if ctx.fs.exists(&staging_base) {
        ctx.fs.remove_dir_all(&staging_base)?;
    }
    ctx.fs.create_dir_all(&staging_base)?;
    crate::doc::write_doc(
        ctx.fs,
        &sp.sync,
        &topos_types::persisted::SyncState {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            observed: 0,
            observed_version_id: crate::ops::inventory::ZERO_HEX.to_owned(),
            applied: 0,
            base_commit: crate::ops::inventory::ZERO_HEX.to_owned(),
            work_hash: crate::ops::inventory::ZERO_HEX.to_owned(),
            held: false,
            draft_observed: None,
        },
    )?;
    crate::doc::write_map(
        ctx.fs,
        &sp.map,
        &topos_types::persisted::PlacementMap {
            schema_version: topos_types::PLACEMENT_MAP_SCHEMA_VERSION,
            placements: Vec::new(),
            applied_commit: crate::ops::inventory::ZERO_HEX.to_owned(),
            materialized_sha: crate::ops::inventory::ZERO_HEX.to_owned(),
            placement_state: Vec::new(),
            harness: Some(ctx.harness.id()),
            harness_slug: Some(ctx.harness.id().slug().to_owned()),
        },
    )?;
    crate::doc::write_doc(ctx.fs, &sp.server, &record)?;
    // lock LAST — the commit marker (recovery keeps a record directory only when it is present).
    crate::doc::write_doc(
        ctx.fs,
        &sp.lock,
        &topos_types::persisted::Lock {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            skill_id: sid.as_str().to_owned(),
            name: name.to_owned(),
            base_commit: crate::ops::inventory::ZERO_HEX.to_owned(),
            bundle_digest: crate::ops::inventory::ZERO_HEX.to_owned(),
            files: Vec::new(),
        },
    )?;
    match ctx
        .fs
        .rename_dir_noreplace(&staging_base, &ctx.layout.skill_dir(sid))
    {
        Ok(()) => {}
        // Raced another run laying the same record — keep theirs, then write our document over it.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            ctx.fs.remove_dir_all(&staging_base)?;
            return crate::doc::write_doc(ctx.fs, &ctx.layout.published(sid).server, &record);
        }
        Err(e) => return Err(ClientError::Io(format!("server record {sid}: {e}"))),
    }
    ctx.fs.fsync_dir(&ctx.layout.skills_dir())?;
    Ok(())
}
