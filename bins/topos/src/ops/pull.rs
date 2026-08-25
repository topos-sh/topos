//! `update`'s per-skill arm — the targeted accept, the go-back, and `--reset`.
//!
//! `topos update <skill>` brings one skill current now instead of waiting for the next sweep;
//! `topos update <skill>@<hash>` puts that version's bytes back on this machine only. (`topos pull`
//! still reaches the same verb — a hidden alias kept for hooks armed by older builds.) The
//! per-skill engine (check → plan → apply) lives in [`super::sync_engine`]; this module is the
//! scope dispatch + aggregation, plus the `--quiet` hook's stdout shapes. Everything the manifest
//! rows demand is converged by [`super::reconcile`] instead.
//!
//! The plane reads ride the real HTTP transport in production; the tests drive the same engine over
//! fixture sources (no HTTP).
//!
//! **The sweep degrades fast when the plane is down.** The first connect-level failure trips a
//! per-invocation circuit breaker ([`BreakerPlane`]): every remaining plane call in this pull
//! short-circuits to an unreachable error the engine already maps to a local-state-only outcome, so a
//! dead plane costs ONE connect timeout, not one per followed skill — the session-start hook must never
//! hang the harness.

use std::cell::Cell;
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use topos_types::persisted::SyncState;
use topos_types::results::{ExchangeFault, PullData, PullSkill, ResetData};

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
    /// over-strict ambiguity refusal. `store` is the SCOPE flag: which store the name resolves in,
    /// and therefore which copy this run acts on.
    One {
        name: String,
        workspace: Option<String>,
        mode: TargetMode,
        store: super::StoreScope,
    },
}

/// How a targeted single-skill pull behaves.
#[derive(Debug)]
pub(crate) enum TargetMode {
    /// `topos pull <skill>` — accept a pending update / resume a held skill / resolve a divergence (no `@hash`).
    AcceptPending,
    /// `topos pull <skill> --keep-mine` — the disclosed exit from a merge that STOPPED: commit the
    /// tree the person settled on as a fresh one-parent commit on the team's version (a 2-way diff
    /// of what it leaves out is surfaced). The team's changes ARE merged in everywhere the two
    /// sides did not collide; only the contested lines keep this person's wording — unless they
    /// edited the folder by hand, in which case that tree is committed exactly as they left it. The
    /// recorded base advances to the team's version, so what remains is an ordinary draft on top of
    /// current, ahead of the team rather than behind it.
    KeepMine,
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
#[derive(Debug)]
pub(crate) struct PullOutcome {
    pub data: PullData,
    /// Isolated per-skill FAILURES — what the receipt counts and calls failed, and what makes the
    /// run exit non-zero. Only [`note_skill_failure`] and its reconcile twin write here.
    pub warnings: Vec<topos_types::Message>,
    /// The BUNDLES this run could not carry forward, keyed `(scope label, bundle identity)`.
    /// `warnings` is a LINE channel and a line is not a bundle — a scope-level fault (an
    /// unavailable lock, an unreadable custody document) is one line about no bundle at all — so
    /// the receipt's arithmetic counts THIS, and the summary can no longer invent bundles that
    /// never existed and report them failed.
    ///
    /// The IDENTITY, never the display name — the same key the sweep's own dedupe and receipt-row
    /// joins use. Scopes are unblended, so one bundle may legitimately fail once per scope; a
    /// workspace `linear` and a local `linear` failing in ONE scope are two bundles and count as
    /// two. Names appear in the warning lines and nowhere else.
    pub failed_bundles: std::collections::BTreeSet<(String, String)>,
    /// The BUNDLES nothing was placed for because nothing COULD be — an MCP server this build or
    /// this machine cannot set up for any engaged agent, or one every agent already holds under an
    /// entry topos does not own. Keyed exactly like `failed_bundles` and counted in its own
    /// summary clause: they are not failures (no act was attempted, so none failed) and they are
    /// not up to date either (nothing of them stands anywhere), and the receipt has to be able to
    /// say the second thing without saying the first.
    pub unplaced_bundles: std::collections::BTreeSet<(String, String)>,
    /// The RUNNABLE fixes for the faults in `warnings`, for the faults that have one — the
    /// `--json` lane's share of what the prose line already tells a person. Empty on a clean run.
    pub fault_actions: Vec<topos_types::NextAction>,
    /// Bundles waiting on a DECISION only the person can make (see [`PendingDecision`]). They are
    /// not failures: the run exits 0 and the receipt counts them under `waiting on you`.
    pub decisions: Vec<PendingDecision>,
    /// ADVISORIES — `warning:` lines about a row that still DELIVERED (an unknown MCP dest entry
    /// dropped from a bundle's narrowing). They join `warnings` in the `--json` envelope's one
    /// stable array and print beside them, but the summary never counts them: the bundle they
    /// annotate has its own row, and counting the line too would invent a second, failed bundle.
    pub advisories: Vec<topos_types::Message>,
    /// Facts about what WORKED that are still worth stating (the settled-draft fan-out, a
    /// cross-scope version split). They join `warnings` in the `--json` envelope's one stable
    /// array, but a successful run must never report itself as having failed anything.
    pub disclosures: Vec<topos_types::Message>,
    /// Workspaces whose whole delivery answered the uniform 404 THIS run (removed / revoked) — every
    /// copy froze in place. Display NAMES: the freeze line and the `update` prose both just say them.
    pub access_gone: Vec<String>,
    /// Workspaces that got NO fresh delivery THIS run (the plane could not be dialed, answered with
    /// a failure, or answered unreadably) — state kept, retry next session; the quiet hook warns
    /// only once the staleness window is blown.
    pub unreachable: Vec<UnreachableWorkspace>,
    /// FINAL refusals discovered this run — a repository the forge says is gone, one sentence
    /// each, said once and then never again. They ride their own channel because the silent
    /// sweep's warnings are discarded: a fact that is only ever stated once must not be stated on
    /// a channel that throws it away.
    pub forge_gone: Vec<String>,
    /// Forges that answered nothing about one or more rows THIS run — the git lane's twin of
    /// `unreachable`, and gated the same way. One entry per HOST, never per row.
    pub stale_forge: Vec<StaleForge>,
    /// `(workspace id, channel name)` expansions that FAILED this run — the destination-receipt
    /// gate reads it: a channel whose member list could not be read proved nothing, whatever
    /// else the same sweep happened to reconcile.
    pub failed_channels: HashSet<(String, String)>,
}

/// One bundle this run left exactly as it was because a DECISION is owed — the person's own edits
/// stand in the way of a newer version, and only they can say which side wins.
///
/// A decision is NOT a failure, and the two must not share a word. Nothing broke, nothing was
/// lost, and no retry will change the answer: what is needed is a person choosing, which is the
/// one thing "failed" never says. So it renders as an ordinary receipt row — the bundle's name,
/// what is standing in the way, and the ways out under it — counts under `waiting on you`, and
/// leaves the run's exit status at 0. A real failure keeps `failed` and a non-zero exit, so an
/// agent that checks the status learns the difference between "something is broken" and "you owe
/// me an answer".
#[derive(Debug, Clone)]
pub(crate) struct PendingDecision {
    /// The bundle's name — the receipt row's left column, padded with every other row's.
    pub name: String,
    /// What is waiting, in one sentence, beside the name.
    pub line: String,
    /// The ways out as the RECEIPT lays them out, one per line, indented beneath the row. Each
    /// producer owns its own layout, because the sentence and the shape are one piece of writing.
    pub detail: Vec<String>,
    /// The SAME ways out as argv — what the `--json` envelope's next actions carry, so an agent
    /// runs the choice instead of parsing the sentence about it. Built from the same tokens the
    /// lines above are printed from.
    pub ways_out: Vec<Vec<String>>,
}

/// One forge host that went quiet this run, and how long its rows have gone unanswered.
///
/// Aggregated by host on purpose. The silent sweep's only channel is text injected into an agent's
/// context window, so its budget is a person's attention: five rows behind one dead network is ONE
/// thing that happened, and it gets one sentence.
#[derive(Debug, Clone)]
pub(crate) struct StaleForge {
    /// The forge (`github.com`) — what the line names, because that is the thing that went quiet.
    pub host: String,
    /// How many of this run's SOURCES sit behind it — repositories, not skills. One repository
    /// can hold any number of skills, so the line must say what it is counting.
    pub sources: usize,
    /// The OLDEST last-answered time across those sources; `None` = one of them has never
    /// answered at all (and so is not stale — there is nothing to be stale from).
    pub answered_at: Option<i64>,
    /// The host ANSWERED at least one of these rows — it just did not answer usefully. A
    /// rate-limited forge and an unreachable one are different things to go look at, and the line
    /// says which.
    pub reached: bool,
}

/// One workspace left without a fresh delivery this run. It carries BOTH halves of the workspace's
/// identity on purpose: the freshness cache (`state/sync_status.json`) is keyed by workspace **id**,
/// while the warning line a person reads must name the workspace the way they know it. Keeping only
/// the name made the staleness lookup miss every time — and a miss reads as "not stale", so the line
/// never printed.
#[derive(Debug)]
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
    /// THE one place these three sentences are written: every later surface that names a fault
    /// (the freshness cache's recorded one, read back by `log`) converts into this enum and asks
    /// here, so no reason can drift into another's clause.
    pub(crate) fn clause(self) -> &'static str {
        match self {
            Self::Unreachable => "the server could not be reached",
            // True of the WHOLE variant: a failure status, an unexpected one, and an answer that
            // never fully arrived. Distinct from the malformed clause, which is about a complete
            // answer whose contents are wrong — a different thing to go look at.
            Self::Unavailable => "the server did not answer successfully",
            Self::Malformed => "the server's answer could not be read",
        }
    }

    /// The DURABLE spelling of this reason — what the freshness cache records for the last exchange.
    pub(crate) fn fault(self) -> ExchangeFault {
        match self {
            Self::Unreachable => ExchangeFault::Unreachable,
            Self::Unavailable => ExchangeFault::Unavailable,
            Self::Malformed => ExchangeFault::Malformed,
        }
    }
}

impl From<ExchangeFault> for StaleReason {
    /// The way back from a recorded fault, so a read of the cache reaches the same [`Self::clause`].
    fn from(fault: ExchangeFault) -> Self {
        match fault {
            ExchangeFault::Unreachable => Self::Unreachable,
            ExchangeFault::Unavailable => Self::Unavailable,
            ExchangeFault::Malformed => Self::Malformed,
        }
    }
}

impl PullOutcome {
    /// Wrap the schema payload with no workspace-level signals (the targeted paths).
    fn plain(
        data: PullData,
        warnings: Vec<topos_types::Message>,
        failed_bundles: BTreeSet<(String, String)>,
    ) -> Self {
        Self {
            data,
            warnings,
            failed_bundles,
            unplaced_bundles: BTreeSet::new(),
            fault_actions: Vec::new(),
            decisions: Vec::new(),
            advisories: Vec::new(),
            disclosures: Vec::new(),
            access_gone: Vec::new(),
            unreachable: Vec::new(),
            stale_forge: Vec::new(),
            forge_gone: Vec::new(),
            failed_channels: HashSet::new(),
        }
    }
}

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
            let mut failed_bundles = BTreeSet::new();
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
                        note_skill_failure(ctx, &mut warnings, &mut failed_bundles, &skill_id, &e);
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
                            // ONE producer for this wording — the reconcile's, so the sweep and
                            // this path can never drift into two spellings of one fact.
                            disclosures.push(super::reconcile::draft_synced_line(
                                &row.skill,
                                row.synced_placements,
                            ));
                        }
                        skills.push(row);
                    }
                    // A hard per-skill failure (corrupt docs, store/io) must not abort the whole sweep —
                    // disclose it (stderr + a typed warning; never stdout, which the hook injects) and
                    // leave that skill put.
                    Err(e) => {
                        note_skill_failure(ctx, &mut warnings, &mut failed_bundles, &skill_id, &e)
                    }
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
                    behind_elsewhere: Vec::new(),
                    triggers: Vec::new(),
                    scope: None,
                },
                warnings,
                failed_bundles,
            );
            out.disclosures = disclosures;
            Ok(out)
        }
        PullScope::One {
            name,
            workspace,
            mode,
            store,
        } => {
            // Resolve in the SCOPE this invocation named: bare acts where you stand (the nearest
            // project store holding the name, else the machine's), `-g` on the machine store alone.
            // A bundle a project `topos.toml` delivers keeps its custody in the checkout's own
            // store, so a targeted update run from inside that checkout drives THAT copy's engine
            // state. Everything the engine reads or writes per skill (locks, docs, the store, the
            // drift scan) rides `sctx` — the owning store's layout. The FOLLOW seam stays the
            // original `ctx`'s: it is machine-level (built from `state/sync_status.json` under
            // `~/.topos/`), and re-rooting it on a project store would read a document that does
            // not exist there.
            let (layout, skill_id, _lock) =
                super::resolve_skill_in_scope(ctx, &name, workspace.as_deref(), store)?;
            let sctx = ctx_with_layout(ctx, &layout);
            // The two modes a CONNECTED SERVER cannot serve, each refused through its own
            // construction: `--keep-mine` finishes a stopped merge over placed files, and such a
            // bundle has none; a go-back puts an earlier version back, and its versions are the
            // catalog's — this machine holds the one it was given and nothing before it.
            if matches!(mode, TargetMode::KeepMine | TargetMode::GoBack(_)) {
                let placements: Vec<String> =
                    doc::read_map(ctx.fs, &layout.published(&skill_id).map)
                        .ok()
                        .flatten()
                        .map(|m: topos_types::persisted::PlacementMap| m.placements)
                        .unwrap_or_default();
                let kind =
                    crate::bundle_kind::classify(&sctx, skill_id.as_str(), &placements).or_skill();
                let refusal = if matches!(mode, TargetMode::KeepMine) {
                    crate::bundle_kind::refuse_file_verb(
                        crate::bundle_kind::FileVerb::KeepMine,
                        &_lock.name,
                        kind,
                    )
                } else {
                    crate::bundle_kind::refuse_version_verb(
                        crate::bundle_kind::VersionVerb::GoBack,
                        &_lock.name,
                        kind,
                    )
                };
                if let Some(refusal) = refusal {
                    return Err(refusal);
                }
            }
            // The go-back and the `--keep-mine` escape are documented plane-independent (the escape is
            // the offline no-deadlock guarantee) — neither spends a network call on the proposals count.
            let plane_independent = matches!(mode, TargetMode::GoBack(_) | TargetMode::KeepMine);
            // Whether the LOCK follows this invocation — decided before `mode` is consumed below.
            let went_back = matches!(mode, TargetMode::GoBack(_));
            let mut row = match mode {
                TargetMode::GoBack(vref) => sync_engine::go_back(&sctx, &skill_id, &vref)?,
                // The escape consults NO follow state: it finishes a stopped merge from the local
                // record and the local store, and a bundle nobody follows can hold one just the
                // same. Routing it through the followed-only sync is what made an explicit
                // `--keep-mine` answer "up to date" for a local path, a forge import, or an
                // unfollowed workspace bundle.
                TargetMode::KeepMine => sync_engine::escape_one(&sctx, &skill_id)?,
                TargetMode::AcceptPending => match ctx
                    .follow
                    .followed()
                    .into_iter()
                    .find(|(id, _)| *id == *skill_id.as_str())
                {
                    Some((_, follow)) if follow.following => sync_engine::sync_one(
                        &sctx,
                        &skill_id,
                        &follow,
                        sync_engine::Invocation::Accept,
                    )?,
                    // Tracked but not followed → there is no `current` to pull; report the local state.
                    _ => sync_engine::current_state(&sctx, &skill_id)?,
                },
            };
            // Stamp the row's workspace provenance from the follow-state (a retained-but-paused entry still
            // resolves; a purely local go-back / tracked-only skill is honestly `None`).
            row.workspace_id = super::followed_workspace(ctx, skill_id.as_str());
            // THE LOCK RECORDS WHAT THE COPY HOLDS. A go-back inside a checkout replaces that
            // checkout's copy with an earlier version — and left `topos.lock` naming the team's,
            // so the file a project COMMITS said the project runs a version no folder there held,
            // and `install --frozen` would have rebuilt exactly the copy the go-back replaced.
            // The entry moves the way a publish or a revert moves it (a pin still holds, and says
            // so), and the receipt names the file.
            let lock_lines: Vec<topos_types::Message> = went_back
                .then(|| go_back_lock_line(ctx, &layout, &skill_id, &row))
                .flatten()
                .into_iter()
                .collect();
            let proposals_awaiting = if plane_independent {
                0
            } else {
                sum_open_proposals(ctx)
            };
            let mut out = PullOutcome::plain(
                PullData {
                    skills: vec![row],
                    proposals_awaiting,
                    notices: Vec::new(),
                    sync: Vec::new(),
                    behind_elsewhere: Vec::new(),
                    triggers: Vec::new(),
                    scope: None,
                },
                Vec::new(),
                BTreeSet::new(),
            );
            out.disclosures = lock_lines;
            Ok(out)
        }
    }
}

/// The cwd project's `topos.lock`, moved to the version a landed GO-BACK now holds — and the line
/// the receipt says it with. `None` when the go-back acted on a store this checkout does not own,
/// when there is no project lock entry to move, or when the copy is not a workspace bundle a lock
/// records.
///
/// Best-effort, exactly like the publish/revert rail it shares: the bytes have already landed, and
/// a lock this run could not move is said on stderr and converged by the next `topos update`.
fn go_back_lock_line(
    ctx: &Ctx<'_>,
    layout: &sidecar::Layout,
    skill_id: &crate::id::SkillId,
    row: &PullSkill,
) -> Option<topos_types::Message> {
    // ONLY the checkout whose OWN copy just moved. A go-back acts on one store: run from inside a
    // project with `-g`, it is the MACHINE's copy that went back, and moving the project's lock
    // then would put a version in the committed file that the checkout's own folder does not hold
    // — the very thing this rail exists to stop.
    let root = layout.project_root()?;
    let roots = ctx.roots.as_ref()?;
    let cwd = roots.cwd.as_deref()?;
    let nearest = crate::manifest::scopes::nearest_manifest_dir(ctx.fs, cwd, Some(&roots.home))?;
    if nearest != root {
        return None;
    }
    // The version the copy HOLDS after the go-back — read from the store the go-back wrote, never
    // from the ref the person typed (a short ref resolved to it; the record is what landed).
    let lock =
        doc::read_doc::<topos_types::persisted::Lock>(ctx.fs, &layout.published(skill_id).lock)
            .ok()
            .flatten()?;
    let ws = super::followed_workspace(ctx, skill_id.as_str())
        .and_then(|id| super::workspace_ref(ctx, &id))?;
    let moved =
        super::advance_project_lock(ctx, &row.skill, &lock.base_commit, &ws.host, &ws.name)?;
    let short = crate::render::short(&moved.version);
    Some(if moved.held {
        crate::message::disclosure(
            "LOCK_HELD",
            format!(
                "{} pins {}, so it keeps {short} — this copy holds {} until the pin changes",
                crate::manifest::MANIFEST_FILE,
                row.skill,
                crate::render::short(&lock.base_commit),
            ),
        )
    } else {
        crate::message::disclosure(
            "LOCK_MOVED",
            format!(
                "{} now records {}@{short} — the version this copy holds",
                crate::manifest::lock::LOCK_FILE,
                row.skill,
            ),
        )
    })
}

/// `update --reset <skill>...` — the loss-led two-phase discard. Refuses without a named skill (a reset
/// throws away local edits; it must never be a blanket "reset everything"). The describe LEADS with the
/// exact draft delta being discarded (the local `diff` — draft vs current); `--yes` discards it, restoring
/// the followed `current` (an imported skill's adopted origin). Resolves ALL-OR-NONE, in the SCOPE the
/// invocation named (`store`) — the copy the describe measures is the copy `--yes` discards.
///
/// `sel` narrows the discard to ONE copy (`-a`/`--dest`). It is the symmetric counterpart of a
/// per-copy publish, and strictly LESS destructive than the whole-bundle reset, which stays exactly
/// as it was: the same two-phase rail, the same loss-led describe, and every edited copy still
/// snapshotted into the store first — only the set of folders rewritten narrows.
///
/// # Errors
/// [`ClientError::InvalidArgument`] with no named skill, or with a selection spread over several
/// skills; name-resolution errors; a store / io failure.
pub(crate) fn reset(
    ctx: &Ctx<'_>,
    targets: &[String],
    yes: bool,
    store: super::StoreScope,
    sel: &super::Selection,
) -> Result<ResetOutcome, ClientError> {
    if targets.is_empty() {
        return Err(ClientError::InvalidArgument(
            "`update --reset` needs a skill name — it discards that skill's local edits; it will not \
             reset every followed skill at once (name the skill: `topos update <skill> --reset`)"
                .into(),
        ));
    }
    if !sel.is_empty() && targets.len() > 1 {
        return Err(ClientError::InvalidArgument(
            "`--dest`/`-a` names ONE copy of ONE skill — name a single skill, or drop the \
             selector to reset every copy of each"
                .into(),
        ));
    }
    // Resolve ALL-OR-NONE in the named scope, keeping each hit's OWNING store beside it: a
    // project-delivered bundle's draft lives in the checkout's own store, and both halves of the
    // reset — the loss disclosure and the discard — must act on that copy, never on a same-named
    // twin in the other scope.
    let mut resolved = Vec::with_capacity(targets.len());
    for token in targets {
        resolved.push(super::resolve_skill_in_scope(ctx, token, None, store)?);
    }
    let mut items = Vec::with_capacity(resolved.len());
    for (layout, id, lock) in &resolved {
        let sctx = ctx_with_layout(ctx, layout);
        let map: topos_types::persisted::PlacementMap =
            doc::read_map(ctx.fs, &layout.published(id).map)?
                .ok_or_else(|| ClientError::Corrupt("missing placement map".to_owned()))?;
        // The KIND decides whether this verb applies AT ALL, and it is asked FIRST — ahead of the
        // selection below, whose vocabulary is skills folders. A config-placed bundle resolved
        // `-a cursor` to the skills dir and refused about a folder that was never the point; now
        // the one kind refusal answers, naming what `--reset` acts on.
        if let Some(refusal) = crate::bundle_kind::refuse_file_verb(
            crate::bundle_kind::FileVerb::Reset,
            &lock.name,
            crate::bundle_kind::classify(&sctx, id.as_str(), &map.placements).or_skill(),
        ) {
            return Err(refusal);
        }
        // WHICH copy this reset acts on, and which edited copies it leaves ALONE — resolved from
        // the same selection the loss diff below is measured through, so the sentences around that
        // diff can never claim a wider loss than the diff shows. Empty selection = the whole
        // bundle: every copy, and nothing left holding edits.
        let picked = if sel.is_empty() {
            None
        } else {
            Some(super::dest_select::select_copy(
                &sctx, sel, &lock.name, &map,
            )?)
        };
        // The draft delta vs current — the exact bytes a reset drops, read from the copy resolved
        // above (never re-resolved: a second pass could answer with the other scope's copy and
        // describe a loss nobody is about to take). Read in the direction the RESET runs: `a` is
        // this copy (what goes away), `b` is the version that lands — a preview that read the
        // other way told a person the opposite of what `--yes` was about to do. DIVERGENT copies
        // cannot render one diff (that freeze is exactly what `--reset` is the named way out of),
        // so the loss is disclosed as the frozen set instead of failing the reset. UNCAPPED
        // deliberately: a loss disclosure must never truncate what would be discarded.
        let drop_diff = match super::reset_preview_diff(
            ctx,
            layout,
            id,
            lock,
            super::DiffBudget::unlimited(),
            sel,
        ) {
            Ok(d) => d.diff,
            Err(e @ ClientError::PlacementsDiverged { .. }) => {
                format!("{e}\n(each copy is snapshotted into the local store before the reset)")
            }
            Err(e) => return Err(e),
        };
        let workspace_id = super::followed_workspace(ctx, id.as_str());
        items.push(ResetData {
            skill: lock.name.clone(),
            workspace: workspace_id
                .as_deref()
                .and_then(|ws| super::workspace_ref(ctx, ws)),
            workspace_id,
            to_version: lock.base_commit.clone(),
            drop_diff,
            applied: false,
            dest: picked.as_ref().map(|p| p.spelling.display.clone()),
            others_kept: picked
                .as_ref()
                .map(|p| p.others_edited.clone())
                .unwrap_or_default(),
            global: store == super::StoreScope::Machine,
            hand_merge: None,
            merge: None,
        });
    }

    if !yes {
        // The apply command is THIS invocation re-spelled: the scope flag rides along, or `--yes`
        // would discard a different copy than the one just described.
        let mut yes_argv = vec!["topos".to_owned(), "update".to_owned()];
        if store == super::StoreScope::Machine {
            yes_argv.push("-g".to_owned());
        }
        yes_argv.extend(targets.iter().cloned());
        // The copy selector rides along for the same reason the scope flag does — without it the
        // apply would discard EVERY copy's edits, not the one described.
        yes_argv.extend(sel.argv_tail());
        yes_argv.push("--reset".to_owned());
        yes_argv.push("--yes".to_owned());
        return Ok(ResetOutcome::Described { items, yes_argv });
    }

    // ---- APPLY (`--yes`) ---- discard each draft back to its base (the draft is snapshotted first),
    // each through ITS owning store — the same copy the describe above measured the loss against.
    for ((layout, id, _lock), item) in resolved.iter().zip(items.iter_mut()) {
        let landed = sync_engine::reset_to_base(&ctx_with_layout(ctx, layout), id, sel)?;
        item.hand_merge = landed.hand_merge;
        // Whether the merge this reset met is over or still standing — the settled-state proof ran
        // inside the reset, so this is the only place that fact can be read from.
        item.merge = match landed.merge {
            sync_engine::ResetMerge::NoneStood => None,
            sync_engine::ResetMerge::StillStopped => {
                Some(topos_types::results::ResetMergeOutcome::StillStopped)
            }
            sync_engine::ResetMerge::Concluded => {
                Some(topos_types::results::ResetMergeOutcome::Concluded)
            }
        };
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
/// never come back. Re-arrival then lands exactly as the original arrival did — the row demanding
/// it is the consent, so the next sweep places its bytes. A skill with no prior sync state needs no
/// reset (it already sits at the baseline).
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
    /// The wire rows — one `(skill_id, applied version)` per held bundle, each in the wire's own
    /// spelling: a file bundle's commit as 64-char hex, a connected server's catalog revision
    /// verbatim.
    pub applied: Vec<(String, String)>,
    /// Every store holding a bundle at a version OTHER than the workspace's current — the
    /// cross-scope split the ONE reported row cannot carry, as DATA: the caller decides which of
    /// them a person is told about, and phrases it.
    pub splits: Vec<VersionSplit>,
}

/// One store holding one bundle at a version that is not the workspace's current.
///
/// Deliberately not pre-formatted. Whether a split is worth a line depends on facts this function
/// does not hold (which scope the run drove, whether the row is pinned there), and the line itself
/// COUNTS bundles rather than naming them — both are the caller's business.
pub(super) struct VersionSplit {
    /// The bundle's opaque plane id — the key the caller resolves to a name.
    pub skill_id: String,
    /// Which store holds it: `None` = the machine's own (person) store, else the project dir.
    pub project_dir: Option<std::path::PathBuf>,
    /// Whether that copy is genuinely BEHIND the workspace's current. `false` when the delivery
    /// answer named no current for the bundle (there is nothing to be behind of) — and `false`
    /// for a copy the store's own sync state says is HELD, which is a deliberate local go-back:
    /// no update command would move it, so nagging about it would nag forever.
    pub behind: bool,
}

/// What this installation HOLDS after the reconcile, over the skills the workspace's deliveries
/// (the feed AND the manifest rows) named: the materialized version from `map.json` (the honest
/// "applied" — a never-received baseline whose bytes have not landed yet has none and is skipped,
/// as is any skill whose placement this sweep removed). COMPLETE-state across stores. Read-only.
///
/// THE PICK IS DETERMINISTIC AND STATED, because the wire carries exactly ONE row per
/// `(session, bundle)`: the PERSON-scope store (the machine's own `~/.topos/`) answers whenever it
/// holds the bundle, and otherwise the project stores answer in ascending order of their project
/// directory path. Nothing depends on which checkout the sweep happened to run from. The
/// deterministic row still stands when the stores disagree, and [`AppliedSnapshot::splits`] carries
/// every store's standing against `current` — the fact the one-row pick would otherwise swallow.
///
/// Scoping to the delivered set is load-bearing: reporting a withdrawn or frozen skill would tell
/// the fleet page this device still serves bytes it does not, and would revive the very detach
/// record the plane wrote.
///
/// `current` is the delivery answer's version per bundle — the ONLY thing that makes "behind"
/// mean anything. A bundle it does not name yields splits that are never `behind`: an unknown
/// current cannot be stood behind.
pub(super) fn applied_snapshot(
    ctx: &Ctx<'_>,
    delivered: &HashSet<&str>,
    project_stores: &[crate::sidecar::Layout],
    current: &std::collections::HashMap<&str, String>,
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
        // row; EVERY one of them (the first included) is measured against the workspace's current,
        // because the store that answers the wire is just as able to be the stale one.
        let mut holdings: Vec<(Option<std::path::PathBuf>, String, bool)> = Vec::new();
        for layout in std::iter::once(&ctx.layout).chain(projects.iter().copied()) {
            let sp = layout.published(&sid);
            // WHAT THIS STORE HOLDS, per kind. A connected server holds a catalog revision and
            // that is the whole of it: no placement dirs, no commit, nothing to scan — the record
            // says which revision, and the per-agent config states ride the same report row.
            let held = match crate::doc::read_doc::<topos_types::persisted::McpServerRecord>(
                ctx.fs, &sp.server,
            )? {
                Some(record) => record.revision_id,
                None => {
                    let Some(map) = doc::read_map(ctx.fs, &sp.map)? else {
                        continue;
                    };
                    // A placement the sweep removed (or never laid) is not held, whatever the doc
                    // says.
                    if map.placements.is_empty()
                        || !map.placements.iter().any(|p| ctx.fs.exists(Path::new(p)))
                    {
                        continue;
                    }
                    match super::parse_hex32(&map.applied_commit) {
                        Ok(commit) if commit != [0u8; 32] => map.applied_commit.clone(),
                        _ => continue,
                    }
                }
            };
            // Behind = not at the served current. A HELD store (a deliberate local go-back)
            // is exempt: no update command would move it, so it is a choice, not staleness.
            let behind = current.get(skill_id).is_some_and(|c| *c != held)
                && !doc::read_doc::<SyncState>(ctx.fs, &sp.sync)?
                    .is_some_and(|s: SyncState| s.held);
            holdings.push((
                layout.project_root().map(std::path::Path::to_path_buf),
                held,
                behind,
            ));
        }
        let Some((_, first_commit, _)) = holdings.first().cloned() else {
            continue;
        };
        applied.push(((*skill_id).to_owned(), first_commit));
        splits.extend(
            holdings
                .into_iter()
                .map(|(project_dir, _, behind)| VersionSplit {
                    skill_id: (*skill_id).to_owned(),
                    project_dir,
                    behind,
                }),
        );
    }
    applied.sort();
    splits.sort_by(|a, b| (&a.skill_id, &a.project_dir).cmp(&(&b.skill_id, &b.project_dir)));
    Ok(AppliedSnapshot { applied, splits })
}

/// One isolated per-skill failure as a stable, machine-parseable envelope warning:
/// `<CODE> <skill_id>: <safe message>` (the same code/safe-message pair the error envelope would carry;
/// the skill id here came from the follow-state, never a secret).
fn skill_warning(skill_id: &str, e: &ClientError) -> topos_types::Message {
    // ONE producer for the per-item failure line — the reconcile's, so the two sweeps cannot
    // spell the same fault differently.
    super::reconcile::item_failure(skill_id, e)
}

/// Disclose one isolated per-skill sweep failure under the same redaction policy as the top-level error
/// path: the SAFE message on stderr (the hook surface — never stdout), the FULL `Display` chain to the
/// append-only diagnostics log (best-effort), and a stable-shape envelope warning.
fn note_skill_failure(
    ctx: &Ctx<'_>,
    warnings: &mut Vec<topos_types::Message>,
    failed: &mut BTreeSet<(String, String)>,
    skill_id: &str,
    e: &ClientError,
) {
    // `(scope label, bundle identity)` — this sweep runs against the MACHINE store alone, so the
    // person scope is the one label it can speak for, and the followed skill id IS the identity.
    failed.insert((
        crate::manifest::scopes::ResolvedScope::Person.label(),
        skill_id.to_owned(),
    ));
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
    crate::out::errln!(
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
        // `<workspace-address>` is a PLACEHOLDER on purpose: `access_gone` carries the display
        // NAME, and a bare name is not something `topos login` can be handed. The trailer the
        // interactive sweep prints spells it the same way, for the same reason.
        lines.push(format!(
            "topos: {ws} — this machine no longer has access (unlinked, removed, or gone). Its \
             skills are frozen in place. Run 'topos login <workspace-address>' to reconnect."
        ));
    }
    // A repository the forge says is GONE is not a staleness nudge — it is a fact about the row
    // that will not change until somebody edits it. Said once, here, whatever else the run had to
    // say: this is the only channel a silent sweep has, and "once" has to mean once out loud.
    lines.extend(out.forge_gone.iter().cloned());
    // A forge that went quiet gets the SAME posture as a workspace that did: recorded always,
    // shown on demand by `status`/`list`, and said here only once the silence has run long enough
    // to mean something. One line names the host, how many skills sit behind it, and — because
    // "unreachable" on its own reads as breakage — that they still work.
    for forge in &out.stale_forge {
        if !crate::forge_check::is_stale(forge.answered_at, now_millis) {
            continue;
        }
        let ago = forge
            .answered_at
            .map(|at| sync_status::human_duration(now_millis.saturating_sub(at)));
        lines.push(format!(
            "topos: {} {} — {} {} not checked{}. They still work.",
            forge.host,
            // A host that answered is not unreachable, whatever else went wrong with the answer.
            if forge.reached {
                "is not answering checks"
            } else {
                "is unreachable"
            },
            forge.sources,
            if forge.sources == 1 {
                "source"
            } else {
                "sources"
            },
            ago.map(|a| format!(" for {a}")).unwrap_or_default(),
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
                "topos: {} last synced {} ago — {}.",
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
        triggers: ctx.triggers.clone(),
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
        triggers: ctx.triggers.clone(),
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
        layout: layout.clone().under_machine(ctx.layout.machine_home()),
        harness: ctx.harness,
        triggers: ctx.triggers.clone(),
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
        layout: layout.clone().under_machine(ctx.layout.machine_home()),
        harness: ctx.harness,
        triggers: ctx.triggers.clone(),
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
