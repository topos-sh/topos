//! `pull` — the session-start auto-update entry point + the targeted accept / go-back.
//!
//! The bare `topos pull` (the installed session-start hook) sweeps every followed skill toward its
//! `current`. A targeted `topos pull <skill>` accepts a pending update for one skill (the explicit
//! command supplies the consent a confirm-each offer solicited); `topos pull <skill>@<hash>` goes back to
//! a specific local version. The per-skill engine (check → plan → apply) lives in
//! [`super::sync_engine`]; this module is the scope dispatch + aggregation.
//!
//! In production the follow-state comes from the enrollment docs (`follows.json`, written by `follow`)
//! and the plane reads ride the real HTTP transport — this is exactly what the installed hook runs. With
//! nothing followed the sweep reports an honestly empty state. The tests drive the same engine over
//! fixture sources (no HTTP).
//!
//! **The sweep degrades fast when the plane is down.** The first connect-level failure trips a
//! per-invocation circuit breaker ([`BreakerPlane`]): every remaining plane call in this pull
//! short-circuits to an unreachable error the engine already maps to a local-state-only outcome, so a
//! dead plane costs ONE connect timeout, not one per followed skill — the session-start hook must never
//! hang the harness.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::Path;

use topos_core::digest::to_hex;
use topos_types::persisted::SyncState;
use topos_types::results::{PullData, ResetData};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::id::SkillId;
use crate::plane::{
    FetchedVersion, FollowSource, KnownCurrent, PlaneError, PlaneSource, PointerFetch,
};
use crate::sync_status::{self};
use crate::{doc, sidecar};

use super::sync_engine::{self};

/// The never-received sentinel the first-receive baseline carries (and an upstream withdrawal
/// restores, so a later re-delivery installs afresh instead of reading as already-current).
const ZERO_GEN: u64 = 0;
const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// What a `pull` invocation targets.
#[derive(Debug)]
pub(crate) enum PullScope {
    /// The bare session-start sweep — every followed skill.
    AllFollowed,
    /// One skill, by name, in a targeted mode. `workspace` pins the resolution to a specific
    /// workspace when a qualified path (`<ws>/skills/<name>`) selected one — so a name shared across
    /// workspaces resolves to exactly the one the user addressed, never a different one or an
    /// over-strict ambiguity refusal.
    One {
        name: String,
        workspace: Option<String>,
        mode: TargetMode,
    },
}

/// How a targeted single-skill pull behaves.
#[derive(Debug)]
pub(crate) enum TargetMode {
    /// `topos pull <skill>` — accept a pending update / resume a held skill / resolve a divergence (no `@hash`).
    AcceptPending,
    /// `topos pull <skill> --onto-current` — the disclosed escape: commit MY bytes on top of `current`,
    /// dropping the merge (a 2-way diff of what is dropped is surfaced). Resolves a divergence without merging.
    OntoCurrent,
    /// `topos pull <skill>@<ref>` — install an older version's bytes locally (a deliberate go-back).
    /// The ref is the full 64-hex id or a short prefix, resolved against the skill's recorded history
    /// inside [`sync_engine::go_back`] (where that history is already loaded and validated).
    GoBack(super::VersionRef),
}

/// A `pull` run's typed result: the per-skill rows PLUS the per-skill hard failures the sweep isolated.
/// `data` is the schema-pinned envelope payload; `warnings` ride the envelope's existing `warnings` field
/// (one stable-shape line per failed skill), so an isolated failure is machine-visible under `--json`
/// instead of stderr-only. `access_gone` / `unreachable` are the STRUCTURED workspace-level signals the
/// hook's quiet posture reads (a freeze line; the staleness warning) — the warnings carry the same facts
/// as prose, but the hook must not parse prose.
pub(crate) struct PullOutcome {
    pub data: PullData,
    /// Isolated per-skill FAILURES — what the receipt counts and calls failed. Only
    /// [`note_skill_failure`] and its reconcile twin write here.
    pub warnings: Vec<String>,
    /// Facts about what WORKED that are still worth stating (the settled-draft fan-out, a
    /// cross-scope version split). They join `warnings` in the `--json` envelope's one stable
    /// array, but a successful run must never report itself as having failed anything.
    pub disclosures: Vec<String>,
    /// Workspaces whose whole delivery answered the uniform 404 THIS run (removed / revoked) — every
    /// copy froze in place. Display NAMES: the freeze line and the `update` prose both just say them.
    pub access_gone: Vec<String>,
    /// Workspaces that got NO fresh delivery THIS run (the plane could not be dialed, answered with
    /// a failure, or answered unreadably) — state kept, retry next session; the quiet hook warns
    /// only once the staleness window is blown.
    pub unreachable: Vec<UnreachableWorkspace>,
}

/// One workspace left without a fresh delivery this run. It carries BOTH halves of the workspace's
/// identity on purpose: the freshness cache (`state/sync_status.json`) is keyed by workspace **id**,
/// while the warning line a person reads must name the workspace the way they know it. Keeping only
/// the name made the staleness lookup miss every time — and a miss reads as "not stale", so the line
/// never printed.
pub(crate) struct UnreachableWorkspace {
    /// The cache key — what `sync_status` records a workspace's last delivery under.
    pub workspace_id: String,
    /// What the person is shown.
    pub workspace_name: String,
    /// What actually went wrong — the line says it instead of blaming the network for all three.
    pub reason: StaleReason,
}

/// Why a workspace got no fresh delivery this run. All three keep local state and retry later, and
/// the staleness nudge ("no fresh data in N days") is equally true of each — but they are DIFFERENT
/// things to the person reading the line, and only the first is a failure to reach the server at
/// all: the other two happen AFTER the plane answered (or began to). Mirrors the
/// [`crate::plane::PlaneError`] variants the sweep degrades on, so the mapping stays checkable at
/// the push site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StaleReason {
    /// Connect-level: the plane could not be dialed at all (dial / TLS / timeout).
    Unreachable,
    /// The plane was reachable but this exchange did not land — it answered with a failure status,
    /// or the answer never fully arrived (a 5xx / unexpected status / a truncated or over-limit
    /// body). Retry later; nothing here says the bytes were wrong, only that they did not get here.
    Unavailable,
    /// A COMPLETE answer arrived and its structure was wrong (corrupt or forged bytes) — a
    /// different thing from a failed exchange, and a different thing to go look at.
    Malformed,
}

impl StaleReason {
    /// The warning line's reason clause — true of what actually happened, in a person's terms.
    /// Pointing someone at their network when the bytes were the problem sends them the wrong way.
    fn clause(self) -> &'static str {
        match self {
            Self::Unreachable => "the server could not be reached",
            // True of the WHOLE variant: a failure status, an unexpected one, and an answer that
            // never fully arrived. Distinct from the malformed clause, which is about a complete
            // answer whose contents are wrong — a different thing to go look at.
            Self::Unavailable => "the server did not answer successfully",
            Self::Malformed => "the server's answer could not be read",
        }
    }
}

impl PullOutcome {
    /// Wrap the schema payload with no workspace-level signals (the targeted paths).
    fn plain(data: PullData, warnings: Vec<String>) -> Self {
        Self {
            data,
            warnings,
            disclosures: Vec::new(),
            access_gone: Vec::new(),
            unreachable: Vec::new(),
        }
    }
}

/// RETIRED-surface placeholder kept for the targeted-pull call shape (the manifest reconcile has
/// its own options).
#[derive(Default)]
pub(crate) struct ReconcileOpts {}

/// Run the update check for `scope`.
///
/// # Errors
/// A hard failure resolving a targeted skill, or (for a targeted pull) a plane-read failure; the bare
/// sweep isolates per-skill failures instead of erroring (each becomes a warning + a stderr line).
pub(crate) fn pull(ctx: &Ctx<'_>, scope: PullScope) -> Result<PullOutcome, ClientError> {
    match scope {
        PullScope::AllFollowed => {
            // The sweep runs through the circuit breaker: the first connect-level failure marks the
            // plane down for the REST of this invocation (including the proposals count below).
            let breaker = BreakerPlane::new(ctx.plane);
            let sweep_ctx = ctx_with_plane(ctx, &breaker);
            let mut skills = Vec::new();
            let mut warnings = Vec::new();
            let mut disclosures = Vec::new();
            for (skill_id, follow) in ctx.follow.followed() {
                if !follow.following {
                    continue;
                }
                // The followed id enters path joins below — parse it like any other boundary id. The
                // enrollment loader already refused a corrupt follows.json, so this only fires for an
                // id that bypassed that load (a fixture / a future source); it is isolated like any
                // other per-skill failure, never a landed path escape.
                let sid = match SkillId::parse(&skill_id) {
                    Ok(sid) => sid,
                    Err(e) => {
                        note_skill_failure(ctx, &mut warnings, &skill_id, &e);
                        continue;
                    }
                };
                match sync_engine::sync_one(
                    &sweep_ctx,
                    &sid,
                    &follow,
                    sync_engine::Invocation::Sweep,
                ) {
                    // Stamp the row's workspace provenance from the skill's OWN follow entry — the sweep
                    // spans skills across every followed workspace, so two same-named skills stay
                    // distinguishable in the `--json` rows.
                    Ok(mut row) => {
                        row.workspace_id = Some(follow.workspace_id.clone());
                        // The settled-draft fan-out's one receipt line — quiet, factual, and a
                        // DISCLOSURE: the fan-out worked, so it never joins the failure count.
                        if row.action == topos_types::results::PullAction::DraftSynced {
                            let n = row.synced_placements.unwrap_or(0);
                            disclosures.push(format!(
                                "DRAFT_SYNCED {skill}: synced your edits of {skill} to {n} other \
                                 agent folder{s}",
                                skill = row.skill,
                                s = if n == 1 { "" } else { "s" }
                            ));
                        }
                        skills.push(row);
                    }
                    // A hard per-skill failure (corrupt docs, store/io) must not abort the whole sweep —
                    // disclose it (stderr + a typed warning; never stdout, which the hook injects) and
                    // leave that skill put.
                    Err(e) => note_skill_failure(ctx, &mut warnings, &skill_id, &e),
                }
            }
            // The proposals count runs AFTER the sweep (it is disclosure, not the update itself) and is skipped
            // entirely once the breaker tripped — no point burning more connect timeouts on it.
            let proposals_awaiting = if breaker.tripped() {
                0
            } else {
                sum_open_proposals(&sweep_ctx)
            };
            let mut out = PullOutcome::plain(
                PullData {
                    skills,
                    proposals_awaiting,
                    notices: Vec::new(),
                    sync: Vec::new(),
                    scope: None,
                },
                warnings,
            );
            out.disclosures = disclosures;
            Ok(out)
        }
        PullScope::One {
            name,
            workspace,
            mode,
        } => {
            let (skill_id, _lock) =
                super::resolve_skill_in_workspace(ctx, &name, workspace.as_deref())?;
            // The go-back and the `--onto-current` escape are documented plane-independent (the escape is
            // the offline no-deadlock guarantee) — neither spends a network call on the proposals count.
            let plane_independent = matches!(mode, TargetMode::GoBack(_) | TargetMode::OntoCurrent);
            let mut row = match mode {
                TargetMode::GoBack(vref) => sync_engine::go_back(ctx, &skill_id, &vref)?,
                TargetMode::AcceptPending | TargetMode::OntoCurrent => {
                    let inv = match mode {
                        TargetMode::OntoCurrent => sync_engine::Invocation::Escape,
                        _ => sync_engine::Invocation::Accept,
                    };
                    match ctx
                        .follow
                        .followed()
                        .into_iter()
                        .find(|(id, _)| *id == *skill_id.as_str())
                    {
                        Some((_, follow)) if follow.following => {
                            sync_engine::sync_one(ctx, &skill_id, &follow, inv)?
                        }
                        // Tracked but not followed → there is no `current` to pull; report the local state.
                        _ => sync_engine::current_state(ctx, &skill_id)?,
                    }
                }
            };
            // Stamp the row's workspace provenance from the follow-state (a retained-but-paused entry still
            // resolves; a purely local go-back / tracked-only skill is honestly `None`).
            row.workspace_id = super::followed_workspace(ctx, skill_id.as_str());
            let proposals_awaiting = if plane_independent {
                0
            } else {
                sum_open_proposals(ctx)
            };
            Ok(PullOutcome::plain(
                PullData {
                    skills: vec![row],
                    proposals_awaiting,
                    notices: Vec::new(),
                    sync: Vec::new(),
                    scope: None,
                },
                Vec::new(),
            ))
        }
    }
}

/// `update --reset <skill>...` — the loss-led two-phase discard. Refuses without a named skill (a reset
/// throws away local edits; it must never be a blanket "reset everything"). The describe LEADS with the
/// exact draft delta being discarded (the local `diff` — draft vs current); `--yes` discards it, restoring
/// the followed `current` (an imported skill's adopted origin). Resolves ALL-OR-NONE.
///
/// # Errors
/// [`ClientError::InvalidArgument`] with no named skill; name-resolution errors; a store / io failure.
pub(crate) fn reset(
    ctx: &Ctx<'_>,
    targets: &[String],
    yes: bool,
) -> Result<ResetOutcome, ClientError> {
    if targets.is_empty() {
        return Err(ClientError::InvalidArgument(
            "`update --reset` needs a skill name — it discards that skill's local edits; it will not \
             reset every followed skill at once (name the skill: `topos update <skill> --reset`)"
                .into(),
        ));
    }
    // Resolve ALL-OR-NONE, then compute each draft delta (the loss the describe leads with).
    let mut resolved = Vec::with_capacity(targets.len());
    for token in targets {
        resolved.push(super::resolve_skill(ctx, token)?);
    }
    let mut items = Vec::with_capacity(resolved.len());
    for (id, lock) in &resolved {
        // The draft delta vs current — the exact bytes a reset drops. DIVERGENT copies cannot render
        // one diff (that freeze is exactly what `--reset` is the named way out of), so the loss is
        // disclosed as the frozen set instead of failing the reset. UNCAPPED deliberately: a loss
        // disclosure must never truncate what would be discarded.
        let drop_diff = match super::diff(ctx, &lock.name, None, super::DiffBudget::unlimited()) {
            Ok(d) => d.diff,
            Err(e @ ClientError::PlacementsDiverged { .. }) => {
                format!("{e}\n(each copy is snapshotted into the local store before the reset)")
            }
            Err(e) => return Err(e),
        };
        items.push(ResetData {
            skill: lock.name.clone(),
            workspace_id: super::followed_workspace(ctx, id.as_str()),
            to_version: lock.base_commit.clone(),
            drop_diff,
            applied: false,
        });
    }

    if !yes {
        let mut yes_argv = vec!["topos".to_owned(), "update".to_owned()];
        yes_argv.extend(targets.iter().cloned());
        yes_argv.push("--reset".to_owned());
        yes_argv.push("--yes".to_owned());
        return Ok(ResetOutcome::Described { items, yes_argv });
    }

    // ---- APPLY (`--yes`) ---- discard each draft back to its base (the draft is snapshotted first).
    for (id, _lock) in &resolved {
        sync_engine::reset_to_base(ctx, id)?;
    }
    for item in &mut items {
        item.applied = true;
    }
    Ok(ResetOutcome::Applied(items))
}

/// The two-phase outcome of `update --reset`.
#[derive(Debug)]
pub(crate) enum ResetOutcome {
    Described {
        items: Vec<ResetData>,
        yes_argv: Vec<String>,
    },
    Applied(Vec<ResetData>),
}

/// Reset a skill's sync state to the NEVER-RECEIVED baseline — the same all-zero sentinel `follow`
/// lays. The reset is what makes a later re-delivery (a curator re-places the skill, an owner
/// unarchives it, a `follow` lifts this device's exclusion) REINSTALL: without it,
/// `applied == observed` and an absent placement read as "already current", and the skill would
/// never come back. Re-arrival then passes the kernel's I-TOFU first-receive consent — an offer,
/// disclosed, exactly as the original arrival was. A skill with no prior sync state needs no reset
/// (it already sits at the baseline).
///
/// # Errors
/// A store/io failure writing the sync doc.
pub(crate) fn reset_to_never_received(
    ctx: &Ctx<'_>,
    sid: &SkillId,
    prior: Option<&SyncState>,
) -> Result<(), ClientError> {
    let sp = ctx.layout.published(sid);
    if let Some(prior) = prior {
        let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, sid)?;
        doc::write_doc(
            ctx.fs,
            &sp.sync,
            &SyncState {
                schema_version: prior.schema_version,
                observed: ZERO_GEN,
                observed_version_id: ZERO_HEX.to_owned(),
                applied: ZERO_GEN,
                base_commit: ZERO_HEX.to_owned(),
                work_hash: ZERO_HEX.to_owned(),
                held: false,
                draft_observed: None,
            },
        )?;
    }
    Ok(())
}

/// The applied report, plus what the single row per bundle could not say.
pub(super) struct AppliedSnapshot {
    /// The wire rows — one `(skill_id, applied commit)` per held bundle.
    pub applied: Vec<(String, [u8; 32])>,
    /// One line per bundle this installation holds at DIFFERENT versions in more than one store —
    /// the cross-scope version split the reported row cannot carry.
    pub splits: Vec<String>,
}

/// What this installation HOLDS after the reconcile, over the skills the workspace's deliveries
/// (the feed AND the manifest rows) named: the materialized version from `map.json` (the honest
/// "applied" — an offered-but-unaccepted first receive has none and is skipped, as is any skill
/// whose placement this sweep removed). COMPLETE-state across stores. Read-only.
///
/// THE PICK IS DETERMINISTIC AND STATED, because the wire carries exactly ONE row per
/// `(session, bundle)`: the PERSON-scope store (the machine's own `~/.topos/`) answers whenever it
/// holds the bundle, and otherwise the project stores answer in ascending order of their project
/// directory path. Nothing depends on which checkout the sweep happened to run from. When more
/// than one store holds the bundle at DIFFERENT versions the deterministic row still stands and
/// [`AppliedSnapshot::splits`] names the split — the same fact `status`'s cross-scope note makes,
/// said where the report is produced rather than swallowed by the first-store-wins loop.
///
/// Scoping to the delivered set is load-bearing: reporting a withdrawn or frozen skill would tell
/// the fleet page this device still serves bytes it does not, and would revive the very detach
/// record the plane wrote.
pub(super) fn applied_snapshot(
    ctx: &Ctx<'_>,
    delivered: &HashSet<&str>,
    project_stores: &[crate::sidecar::Layout],
) -> Result<AppliedSnapshot, ClientError> {
    // The stated order: the person store, then the project stores by path. `recall_and_record`
    // already yields a path-sorted set; sorting here makes the guarantee this function's own.
    let mut projects: Vec<&crate::sidecar::Layout> = project_stores.iter().collect();
    projects.sort_by_key(|l| l.home().to_path_buf());

    let mut applied = Vec::new();
    let mut splits = Vec::new();
    for skill_id in delivered {
        let Ok(sid) = SkillId::parse(skill_id) else {
            continue;
        };
        // Every store that genuinely holds it, in the stated order — the first is the reported
        // row, the rest exist only to disclose a version split.
        let mut holdings: Vec<(String, [u8; 32])> = Vec::new();
        for layout in std::iter::once(&ctx.layout).chain(projects.iter().copied()) {
            let sp = layout.published(&sid);
            let Some(map) = doc::read_map(ctx.fs, &sp.map)? else {
                continue;
            };
            // A placement the sweep removed (or never laid) is not held, whatever the doc says.
            if !map.placements.iter().any(|p| ctx.fs.exists(Path::new(p))) {
                continue;
            }
            if let Ok(commit) = super::parse_hex32(&map.applied_commit)
                && commit != [0u8; 32]
            {
                let where_ = layout
                    .project_root()
                    .map_or_else(|| "this machine".to_owned(), |d| d.display().to_string());
                holdings.push((where_, commit));
            }
        }
        let Some((first_where, first_commit)) = holdings.first().cloned() else {
            continue;
        };
        applied.push(((*skill_id).to_owned(), first_commit));
        let mut differing: Vec<String> = holdings
            .iter()
            .skip(1)
            .filter(|(_, c)| *c != first_commit)
            .map(|(w, c)| format!("{w} holds {}", short_hex(c)))
            .collect();
        if !differing.is_empty() {
            differing.sort();
            differing.dedup();
            splits.push(format!(
                "VERSION_SPLIT {skill_id}: reported as {first_where}'s {} — {}; nothing blends, \
                 so each scope keeps its own copy",
                short_hex(&first_commit),
                differing.join("; ")
            ));
        }
    }
    applied.sort();
    splits.sort();
    Ok(AppliedSnapshot { applied, splits })
}

/// A version as a disclosure line spells it.
fn short_hex(commit: &[u8; 32]) -> String {
    to_hex(commit).get(..12).unwrap_or_default().to_owned()
}

/// One isolated per-skill failure as a stable, machine-parseable envelope warning:
/// `<CODE> <skill_id>: <safe message>` (the same code/safe-message pair the error envelope would carry;
/// the skill id here came from the follow-state, never a secret).
fn skill_warning(skill_id: &str, e: &ClientError) -> String {
    format!(
        "{} {skill_id}: {}",
        e.code(),
        crate::render::safe_message(e)
    )
}

/// Disclose one isolated per-skill sweep failure under the same redaction policy as the top-level error
/// path: the SAFE message on stderr (the hook surface — never stdout), the FULL `Display` chain to the
/// append-only diagnostics log (best-effort), and a stable-shape envelope warning.
fn note_skill_failure(ctx: &Ctx<'_>, warnings: &mut Vec<String>, skill_id: &str, e: &ClientError) {
    let _ = crate::logfile::append_error_event(
        ctx.fs,
        &ctx.layout.log_path(),
        "pull",
        e.code(),
        &format!("skill {skill_id}: {}", e.detail()),
        // First-class, so `topos log <skill>`'s skill_id filter surfaces the wedged skill's failures.
        Some(skill_id),
        ctx.clock.now_unix_millis(),
    );
    eprintln!(
        "topos pull: skill {skill_id}: {}",
        crate::render::safe_message(e)
    );
    warnings.push(skill_warning(skill_id, e));
}

/// The quiet hook's stdout lines — the ONLY bytes `update --quiet` may emit on stdout (which the
/// session-start hook injects into the session), and only for the two facts a person must not miss:
///
/// - a workspace whose access is GONE this run (removed / revoked) — one line naming the freeze;
/// - a workspace that got NO fresh delivery this run AND whose last successful delivery is older
///   than its staleness window — one line with the age and an honest reason (a fresh miss stays
///   silent: transient blips must not spam every session).
///
/// Reads the freshness doc best-effort (an unreadable one warns nowhere — the hook stays silent
/// rather than noisy).
pub(crate) fn quiet_hook_lines(
    fs: &dyn crate::fs_seam::FsOps,
    layout: &crate::sidecar::Layout,
    now_millis: i64,
    out: &PullOutcome,
) -> Vec<String> {
    let mut lines = Vec::new();
    for ws in &out.access_gone {
        lines.push(format!(
            "topos: {ws} — this device no longer has access (unlinked, removed, or gone); its \
             skills are frozen in place"
        ));
    }
    if out.unreachable.is_empty() {
        return lines;
    }
    let status = sync_status::read(fs, layout).unwrap_or_default();
    for ws in &out.unreachable {
        // Keyed by ID (what the cache records under), rendered by NAME (what the person knows).
        let entry = status.workspaces.get(&ws.workspace_id);
        if sync_status::is_stale(entry, now_millis) {
            let last = entry.and_then(|e| e.last_delivery_at).unwrap_or(now_millis);
            // The nudge (the shared prefix) is the same fact for every kind; only the reason
            // clause varies, so each line is TRUE of what happened.
            lines.push(format!(
                "topos: {} last synced {} ago — {}",
                ws.workspace_name,
                sync_status::human_duration(now_millis.saturating_sub(last)),
                ws.reason.clause()
            ));
        }
    }
    lines
}

/// Whether a failed `update --quiet` exits 0 with a one-line warning instead of nonzero: the hook
/// posture — an AUTH or TRANSPORT failure must never fail the session start (the harness would
/// surface a scary error for a network blip), while a genuinely local failure (corrupt sidecar,
/// io) still exits nonzero so it is not silently swallowed forever.
pub(crate) fn quiet_soft_failure(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Plane(_)
            | ClientError::Enrollment(_)
            | ClientError::SessionRequired { .. }
            | ClientError::NotAvailable(_)
            | ClientError::PlaneRejected(_)
            | ClientError::PlaneTerminal { .. }
            | ClientError::Denied(_)
            | ClientError::TargetNotFound { .. }
    )
}

/// A shallow copy of `ctx` with the plane source swapped (the breaker wraps the real transport for
/// the duration of one sweep; the `follow --yes` reconcile swaps its own delivery transport in; every
/// other seam is shared).
pub(super) fn ctx_with_plane<'a>(ctx: &'a Ctx<'a>, plane: &'a dyn PlaneSource) -> Ctx<'a> {
    Ctx {
        fs: ctx.fs,
        ids: ctx.ids,
        clock: ctx.clock,
        device_id: ctx.device_id.clone(),
        layout: ctx.layout.clone(),
        harness: ctx.harness,
        plane,
        follow: ctx.follow,
        roots: ctx.roots.clone(),
        progress: ctx.progress,
    }
}

/// A shallow copy of `ctx` with BOTH the plane source AND the follow seam swapped — the re-attach
/// reconcile drives the delivery transport for its byte fetches (`bind_skill` must land on the object
/// the fetches use) AND a follow seam re-read from disk (the startup seam predates the re-attach's
/// `set_following` / `set_excluded` writes, so a just-re-affirmed skill would otherwise read as paused).
pub(super) fn ctx_with_plane_and_follow<'a>(
    ctx: &'a Ctx<'a>,
    plane: &'a dyn PlaneSource,
    follow: &'a dyn FollowSource,
) -> Ctx<'a> {
    Ctx {
        fs: ctx.fs,
        ids: ctx.ids,
        clock: ctx.clock,
        device_id: ctx.device_id.clone(),
        layout: ctx.layout.clone(),
        harness: ctx.harness,
        plane,
        follow,
        roots: ctx.roots.clone(),
        progress: ctx.progress,
    }
}

/// A shallow copy of `ctx` running against a different STORE layout (a project's own store) —
/// every other seam shared. The layout IS the scope: the whole engine (locks, recovery, doc IO,
/// the sync machine) runs unchanged against whichever store the ctx carries.
pub(crate) fn ctx_with_layout<'a>(ctx: &'a Ctx<'a>, layout: &crate::sidecar::Layout) -> Ctx<'a> {
    Ctx {
        fs: ctx.fs,
        ids: ctx.ids,
        clock: ctx.clock,
        device_id: ctx.device_id.clone(),
        layout: layout.clone(),
        harness: ctx.harness,
        plane: ctx.plane,
        follow: ctx.follow,
        roots: ctx.roots.clone(),
        progress: ctx.progress,
    }
}

/// [`ctx_with_plane_and_follow`] against an explicit STORE layout — the reconcile's per-item entry
/// (the scope's store + the item's session lane + the run's follow seam, in one hop).
pub(super) fn ctx_with_store<'a>(
    ctx: &'a Ctx<'a>,
    layout: &crate::sidecar::Layout,
    plane: &'a dyn PlaneSource,
    follow: &'a dyn FollowSource,
) -> Ctx<'a> {
    Ctx {
        fs: ctx.fs,
        ids: ctx.ids,
        clock: ctx.clock,
        device_id: ctx.device_id.clone(),
        layout: layout.clone(),
        harness: ctx.harness,
        plane,
        follow,
        roots: ctx.roots.clone(),
        progress: ctx.progress,
    }
}

/// The per-invocation plane circuit breaker. Delegates to the real source until the first
/// **connect-level** failure ([`PlaneError::Unreachable`] — the plane could not be dialed at all), then
/// answers every remaining call with an immediate `Unreachable` so a sweep over N skills costs one
/// connect timeout, not N (and the proposals count is skipped entirely). An HTTP-level failure
/// ([`PlaneError::Unavailable`], e.g. a 500 on one skill) never trips it — the plane answered.
struct BreakerPlane<'a> {
    inner: &'a dyn PlaneSource,
    down: Cell<bool>,
}

impl<'a> BreakerPlane<'a> {
    fn new(inner: &'a dyn PlaneSource) -> Self {
        Self {
            inner,
            down: Cell::new(false),
        }
    }

    fn tripped(&self) -> bool {
        self.down.get()
    }

    fn short_circuit(&self) -> PlaneError {
        PlaneError::Unreachable(
            "the server was already unreachable earlier in this run — the remaining calls were \
             skipped"
                .into(),
        )
    }

    fn note<T>(&self, r: Result<T, PlaneError>) -> Result<T, PlaneError> {
        if matches!(r, Err(PlaneError::Unreachable(_))) {
            self.down.set(true);
        }
        r
    }
}

impl PlaneSource for BreakerPlane<'_> {
    fn get_current(
        &self,
        skill_id: &str,
        known: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError> {
        if self.down.get() {
            return Err(self.short_circuit());
        }
        self.note(self.inner.get_current(skill_id, known))
    }

    fn fetch_version(
        &self,
        skill_id: &str,
        version_id: [u8; 32],
    ) -> Result<FetchedVersion, PlaneError> {
        if self.down.get() {
            return Err(self.short_circuit());
        }
        self.note(self.inner.fetch_version(skill_id, version_id))
    }

    fn list_open_proposals(&self, skill_id: &str) -> Result<Vec<[u8; 32]>, PlaneError> {
        if self.down.get() {
            return Err(self.short_circuit());
        }
        self.note(self.inner.list_open_proposals(skill_id))
    }
}

/// The count of OPEN proposals across the FOLLOWED skills (the `proposals_awaiting` figure) — sourced from
/// the plane's proposals read route, one GET per followed skill. **Best-effort:** a per-skill read failure
/// contributes `0` and never aborts the pull (and never writes to stdout — the session-start hook injects
/// stdout). Runs after the sweep, through the same breaker, so a down plane costs it nothing.
fn sum_open_proposals(ctx: &Ctx<'_>) -> u32 {
    ctx.follow
        .followed()
        .into_iter()
        .filter(|(_, f)| f.following)
        .map(|(id, _)| {
            ctx.plane
                .list_open_proposals(&id)
                .map(|p| u32::try_from(p.len()).unwrap_or(u32::MAX))
                .unwrap_or(0)
        })
        .fold(0u32, u32::saturating_add)
}
