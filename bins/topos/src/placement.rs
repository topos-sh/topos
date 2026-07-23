//! The **placement engine** — WHERE a followed skill's bytes land on this machine, computed from the
//! machine (which agents are detected, which read the shared `~/.agents/skills` convention dir) and
//! the skill's device-local agent scope (`follow --agent` / `unfollow --agent`).
//!
//! ## The policy: shared-dir-first
//!
//! - An **UNSCOPED** skill (no include-list, no per-agent exclusions) lands ONE copy in the shared
//!   cross-agent dir when at least one detected harness is covered by it
//!   ([`topos_harness::coverage`]), PLUS one native copy per detected harness the shared dir does
//!   NOT cover. With no harness detected at all (or no machine roots injected), the classic behavior
//!   holds: the active adapter's single placement.
//! - A **SCOPED** skill (a non-empty include-list and/or per-agent exclusions) lands native copies in
//!   exactly the scoped-and-not-excluded DETECTED harnesses — NEVER the shared dir (a shared dir
//!   cannot express narrowing).
//!
//! ## Target-set reconciliation
//!
//! Targets are recomputed each sync. A NEW target (a newly detected harness, newly true coverage, a
//! scope change) is APPENDED to the map with no materialized bytes yet and lands on the next apply. A
//! placement LEAVES the record only through an explicit verb (`remove --agent` / `unfollow --agent` /
//! a scope change), which cleans its dir snapshot-first; detection loss alone never deletes a byte —
//! the recorded copy freezes in place, unmanaged (skipped by the apply, kept on disk).
//!
//! ## Naming + never-clobber
//!
//! Every new target dir is named by the ONE discipline the reference adapter uses
//! ([`topos_harness::choose_skill_dir`]): the sanitized display name, workspace-suffixed on a
//! collision (`<name>-<ws>`), the validated id as the last resort — and only a FREE dir or one this
//! skill's own placement record already owns is ever chosen. An already-recorded (kind, agent)
//! target keeps its dir verbatim (stability comes from the record, not from re-derivation). One
//! CLI-side refinement on top: an occupant that is a byte-identical copy of the incoming version —
//! and that no OTHER tracked skill's record owns — is ADOPTED in place (the caller arms the choice
//! with the incoming bundle digest), never duplicated under a namespaced sibling.

use std::path::{Path, PathBuf};

use topos_harness::coverage;
use topos_harness::{PlacementNaming, registry};
use topos_types::persisted::{Lock, PlacementKind, PlacementMap, PlacementState, SwapCapability};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::scan::{self, ScannedBundle};
use crate::stat_cache;

/// One planned placement target — where one copy of the skill's bytes belongs on this machine.
#[derive(Debug, Clone)]
pub(crate) struct PlannedTarget {
    pub dir: PathBuf,
    pub kind: PlacementKind,
    /// The registry slug a `Native` target serves (`None` for the shared dir).
    pub agent: Option<String>,
}

/// One covered harness riding the shared target (describe disclosure: which agents the one shared
/// copy reaches, and whether that claim is vendor-docs-level rather than live-probed).
#[derive(Debug, Clone)]
pub(crate) struct CoveredAgent {
    pub slug: String,
    pub docs_level: bool,
}

/// The full placement plan for one skill on this machine.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlacementPlan {
    pub targets: Vec<PlannedTarget>,
    /// The harnesses the shared target covers (empty when no shared target is planned).
    pub shared_covers: Vec<CoveredAgent>,
}

/// The device-local agent scope a plan narrows by (the skill's `follows.json` fields).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AgentScope<'a> {
    /// The include-list (`follow --agent`); empty = unscoped.
    pub agents: &'a [String],
    /// The per-agent exclusions (`unfollow --agent` / `remove --agent`).
    pub excluded: &'a [String],
}

impl AgentScope<'_> {
    /// Whether this scope narrows placement at all (either list non-empty ⇒ native-only mode).
    pub(crate) fn narrows(&self) -> bool {
        !self.agents.is_empty() || !self.excluded.is_empty()
    }
}

/// The device-local agent scope recorded for `skill_id`, from the follow-state seam (empty scope for
/// a purely local / unfollowed skill).
pub(crate) fn scope_of(ctx: &Ctx<'_>, skill_id: &str) -> (Vec<String>, Vec<String>) {
    ctx.follow
        .followed()
        .into_iter()
        .find(|(id, _)| id == skill_id)
        .map(|(_, fc)| (fc.agents, fc.excluded_agents))
        .unwrap_or_default()
}

/// Compute the placement plan for one skill. `naming` carries the untrusted display name + workspace
/// slug (the collision namespace); `prior` is the durable record whose dirs are kept verbatim for
/// already-recorded targets; `adopt` is the INCOMING version's bundle digest, arming adopt-in-place
/// (a first receive passes it so a byte-identical occupant becomes the placement instead of a
/// namespaced sibling — and a recorded adoption reservation is reusable only for THAT digest).
/// With no roots (no `$HOME`, or a test that does not exercise detection) or no detected harness,
/// the plan is the CLASSIC single placement — the prior record as-is, else the active adapter's
/// placement.
pub(crate) fn plan_targets(
    ctx: &Ctx<'_>,
    skill_id: &str,
    naming: PlacementNaming<'_>,
    scope: AgentScope<'_>,
    prior: Option<&PlacementMap>,
    adopt: Option<[u8; 32]>,
) -> PlacementPlan {
    let detected: Vec<&'static registry::KnownHarness> = match &ctx.roots {
        Some(roots) => registry::detected_harnesses(&roots.home, roots.cwd.as_deref()),
        None => Vec::new(),
    };
    if detected.is_empty() {
        return classic_plan(ctx, skill_id, naming, prior, adopt);
    }
    let home = &ctx
        .roots
        .as_ref()
        .expect("detected harnesses imply roots")
        .home;
    let cwd = ctx.roots.as_ref().and_then(|r| r.cwd.as_deref());

    // The eligible slugs: detected, narrowed by the include-list, minus the exclusions. A slug the
    // include-list names but the machine does not detect contributes no target (the verb surface
    // already disclosed "placement engages when the harness is detected").
    let eligible: Vec<&'static registry::KnownHarness> = detected
        .into_iter()
        .filter(|h| scope.agents.is_empty() || scope.agents.iter().any(|a| a == h.slug))
        .filter(|h| !scope.excluded.iter().any(|a| a == h.slug))
        .collect();

    let owned = owned_predicate(prior);
    // The CLI's taken-probe: a path is unavailable when a filesystem entry holds it (lstat, so a
    // dangling symlink counts) OR when ANOTHER tracked skill's record names it — an absent-on-disk
    // recorded path stays reserved for its owner, so the ladder suffixes past it exactly like an
    // occupied dir.
    let taken =
        |p: &Path| topos_harness::dir_taken(p) || recorded_by_another_skill(ctx, skill_id, p);
    let mut plan = PlacementPlan::default();

    // Shared-dir-first — but ONLY for an unscoped skill: a shared dir cannot express narrowing, so
    // any include-list or exclusion forces native-only placement.
    let mut native: Vec<&'static registry::KnownHarness> = Vec::new();
    if scope.narrows() {
        native = eligible;
    } else {
        for h in eligible {
            let support = coverage::shared_dir_support(h.slug);
            if support.covered() {
                plan.shared_covers.push(CoveredAgent {
                    slug: h.slug.to_owned(),
                    docs_level: support.docs_level(),
                });
            } else {
                native.push(h);
            }
        }
        if !plan.shared_covers.is_empty() {
            let dir = prior_dir(prior, PlacementKind::Shared, None, adopt).unwrap_or_else(|| {
                adopt_override(
                    ctx,
                    topos_harness::choose_skill_dir(
                        &coverage::shared_skills_dir(home),
                        skill_id,
                        naming,
                        &taken,
                        &owned,
                    ),
                    skill_id,
                    naming,
                    adopt,
                )
            });
            plan.targets.push(PlannedTarget {
                dir,
                kind: PlacementKind::Shared,
                agent: None,
            });
        }
    }

    let active_slug = ctx.harness.id().slug();
    for h in native {
        let dir = match prior_dir(prior, PlacementKind::Native, Some(h.slug), adopt) {
            Some(dir) => dir,
            // The active adapter keeps its own richer `placement_for` for its native dir; every
            // other detected harness resolves through the registry's canonical user skills root
            // (v0's composition root constructs one adapter — the registry root is the same dir the
            // sibling adapters' no-discovery default names) with the shared naming discipline.
            None if h.slug == active_slug => {
                adapter_choice(ctx, skill_id, naming, &taken, &owned, adopt)
            }
            None => {
                let Some(root) =
                    registry::skills_root(h.slug, registry::SkillScope::User, home, cwd)
                else {
                    continue; // a cwd-only harness has no user-scope dir — nothing to place
                };
                adopt_override(
                    ctx,
                    topos_harness::choose_skill_dir(&root, skill_id, naming, &taken, &owned),
                    skill_id,
                    naming,
                    adopt,
                )
            }
        };
        // A native dir may coincide with an already-planned target (a harness whose native user dir
        // IS the shared convention dir, placed under a scope) — one dir, one copy, one record.
        if plan.targets.iter().any(|t| t.dir == dir) {
            continue;
        }
        plan.targets.push(PlannedTarget {
            dir,
            kind: PlacementKind::Native,
            agent: Some(h.slug.to_owned()),
        });
    }

    // The AGENT-LESS recorded placements — an adopt-in-place source dir, a plain tracked dir with no
    // known harness — are ALWAYS managed: they are the user's own chosen location (often the author's
    // working copy), and neither detection nor an agent scope speaks for them.
    if let Some(map) = prior {
        for (dir, st) in map.placements.iter().zip(&map.placement_state) {
            if st.kind == PlacementKind::Native
                && st.agent.is_none()
                && !plan.targets.iter().any(|t| t.dir == Path::new(dir))
            {
                plan.targets.push(PlannedTarget {
                    dir: PathBuf::from(dir),
                    kind: PlacementKind::Native,
                    agent: None,
                });
            }
        }
    }

    if plan.targets.is_empty() {
        // Nothing eligible (everything excluded / an include-list of undetected slugs): the skill
        // keeps its record but nothing is managed — the caller's apply set is empty. The classic
        // fallback deliberately does NOT engage here: an explicit scope that excludes everything
        // must not resurrect the active adapter's copy.
        if !scope.narrows() {
            return classic_plan(ctx, skill_id, naming, prior, adopt);
        }
    }
    plan
}

/// The classic single-placement plan (no detection): the prior record's targets as-is — except a
/// STALE adoption reservation (never materialized, dir occupied, the occupant no longer matching
/// its recorded digest), which is re-chosen fresh so [`reconcile_map`] can replace it (mirroring
/// [`prior_dir`]'s validation on the detection path; returned verbatim it would wedge every apply
/// on the never-clobber refusal) — else the active adapter's placement.
fn classic_plan(
    ctx: &Ctx<'_>,
    skill_id: &str,
    naming: PlacementNaming<'_>,
    prior: Option<&PlacementMap>,
    adopt: Option<[u8; 32]>,
) -> PlacementPlan {
    let owned = owned_predicate(prior);
    let taken =
        |p: &Path| topos_harness::dir_taken(p) || recorded_by_another_skill(ctx, skill_id, p);
    if let Some(map) = prior
        && !map.placements.is_empty()
    {
        let mut targets: Vec<PlannedTarget> = Vec::new();
        let mut invalid: Vec<(PlacementKind, Option<String>)> = Vec::new();
        for (dir, st) in map.placements.iter().zip(&map.placement_state) {
            let reusable = st.materialized_sha.is_some()
                || !topos_harness::dir_taken(Path::new(dir))
                || adoption_reservation_holds(dir, st, adopt);
            if reusable {
                targets.push(PlannedTarget {
                    dir: PathBuf::from(dir),
                    kind: st.kind,
                    agent: st.agent.clone(),
                });
            } else {
                invalid.push((st.kind, st.agent.clone()));
            }
        }
        // Each invalidated key re-chooses through the active adapter (+ the caller's adopt digest);
        // an equal dir already planned collapses to one target.
        for (kind, agent) in invalid {
            let dir = adapter_choice(ctx, skill_id, naming, &taken, &owned, adopt);
            if targets.iter().any(|t| t.dir == dir) {
                continue;
            }
            targets.push(PlannedTarget { dir, kind, agent });
        }
        return PlacementPlan {
            targets,
            shared_covers: Vec::new(),
        };
    }
    PlacementPlan {
        targets: vec![PlannedTarget {
            dir: adapter_choice(ctx, skill_id, naming, &taken, &owned, adopt),
            kind: PlacementKind::Native,
            agent: Some(ctx.harness.id().slug().to_owned()),
        }],
        shared_covers: Vec::new(),
    }
}

/// The active adapter's placement, hardened against paths ANOTHER tracked skill's record owns: the
/// adapter's own ladder probes only the filesystem (it is content-blind and cannot see the sidecar),
/// so when its answer lands on a recorded-elsewhere path — occupied or deleted — the ONE ladder
/// re-runs directly over the same root with the CLI's full taken-probe (no duplicate naming logic;
/// the adapter told us the root). The adopt override then applies as usual.
fn adapter_choice(
    ctx: &Ctx<'_>,
    skill_id: &str,
    naming: PlacementNaming<'_>,
    taken: &dyn Fn(&Path) -> bool,
    owned: &dyn Fn(&Path) -> bool,
    adopt: Option<[u8; 32]>,
) -> PathBuf {
    let mut dir = ctx.harness.placement_for(skill_id, naming, None).dir;
    if recorded_by_another_skill(ctx, skill_id, &dir)
        && let Some(root) = dir.parent().map(Path::to_path_buf)
    {
        dir = topos_harness::choose_skill_dir(&root, skill_id, naming, taken, owned);
    }
    adopt_override(ctx, dir, skill_id, naming, adopt)
}

/// Adopt-in-place override on a chosen placement dir: when the by-name candidate under the same
/// root is OCCUPIED by a byte-identical copy of the incoming version — and no OTHER tracked
/// skill's record already owns that dir — that dir IS the placement, never a second namespaced/id
/// copy. The reserved built-in dir name is never overridden. When the naming discipline already
/// chose the by-name dir (free, or owned), the candidate equals the choice and nothing changes.
fn adopt_override(
    ctx: &Ctx<'_>,
    dir: PathBuf,
    skill_id: &str,
    naming: PlacementNaming<'_>,
    adopt: Option<[u8; 32]>,
) -> PathBuf {
    let Some(digest) = adopt else {
        return dir;
    };
    let Some(name) = naming.name.and_then(topos_harness::sanitize_skill_dir) else {
        return dir;
    };
    if name == topos_harness::RESERVED_SKILL_DIR && skill_id != topos_harness::RESERVED_SKILL_DIR {
        return dir;
    }
    let Some(parent) = dir.parent() else {
        return dir;
    };
    let candidate = parent.join(&name);
    if candidate != dir
        && topos_harness::dir_taken(&candidate)
        && !recorded_by_another_skill(ctx, skill_id, &candidate)
        && digest_probe(digest)(&candidate)
    {
        candidate
    } else {
        dir
    }
}

/// The dir the prior record holds for a (kind, agent) key — target stability comes from the record.
/// A record that was NEVER materialized and whose dir has since been occupied by someone else is not
/// reusable (never clobber a foreign dir): the key re-chooses, and [`reconcile_map`] replaces the
/// stale reservation. ONE occupied-but-reusable exception: an ADOPTION RESERVATION — a
/// never-materialized placement whose `pre_existing_sha` records the occupant's digest, laid at
/// first-receive baseline time for a byte-identical occupant — stays reusable while the occupant is
/// unchanged (a fresh scan reproduces the recorded sha) AND, when the caller supplied the incoming
/// version's digest, that version is still the recorded one (an adoption recorded for version A is
/// never reused for an apply of version B). A failed scan or any mismatch means the reservation
/// lapsed: NOT reusable, and it is replaced like any other stale reservation.
fn prior_dir(
    prior: Option<&PlacementMap>,
    kind: PlacementKind,
    agent: Option<&str>,
    adopt: Option<[u8; 32]>,
) -> Option<PathBuf> {
    let map = prior?;
    map.placements
        .iter()
        .zip(&map.placement_state)
        .find(|(dir, st)| {
            st.kind == kind
                && st.agent.as_deref() == agent
                && (st.materialized_sha.is_some()
                    || !topos_harness::dir_taken(Path::new(dir))
                    || adoption_reservation_holds(dir, st, adopt))
        })
        .map(|(dir, _)| PathBuf::from(dir))
}

/// Whether a never-materialized-but-occupied placement is a still-valid ADOPTION reservation: its
/// recorded `pre_existing_sha` (the adopted occupant's digest) still matches a fresh scan of the
/// dir, and — when the caller supplied the incoming version's digest — that digest too. Fails
/// closed — an unscannable or changed occupant, or a version the adoption was not recorded for, is
/// not ours to reuse.
fn adoption_reservation_holds(dir: &str, st: &PlacementState, adopt: Option<[u8; 32]>) -> bool {
    st.pre_existing_sha.as_deref().is_some_and(|sha| {
        adopt.is_none_or(|d| topos_core::digest::to_hex(&d) == sha)
            && scan::scan(Path::new(dir))
                .is_ok_and(|s| topos_core::digest::to_hex(&s.bundle_digest) == sha)
    })
}

/// The standard adopt-in-place content probe over an incoming/current bundle digest: an occupant
/// whose scanned bytes digest-equal it is adoptable.
fn digest_probe(digest: [u8; 32]) -> impl Fn(&Path) -> bool {
    move |p: &Path| scan::scan(p).is_ok_and(|s| s.bundle_digest == digest)
}

/// A path's canonical form for record comparison, resolvable even when the LEAF is absent: a
/// deleted placement dir still compares by canonicalizing its (real) parent and re-joining the
/// leaf — records store canonicalized paths, and a deleted dir must stay comparable or its
/// reservation would silently stop matching.
fn canonical_or_parent(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok().or_else(|| {
        let parent = p.parent()?;
        let leaf = p.file_name()?;
        std::fs::canonicalize(parent).ok().map(|c| c.join(leaf))
    })
}

/// Whether ANOTHER tracked skill's placement map records `dir` (materialized or reserved,
/// PRESENT on disk or deleted — an absent recorded path stays reserved for its owner) — a
/// candidate some other skill's record names must never be claimed or adopted, or two records
/// would own one path. Best-effort over the sidecar walk (naming-choice moments are rare, so the
/// cost is fine): an absent sidecar records nothing; an unreadable entry is skipped — the
/// materializer's never-clobber backstop stays the hard rail behind this planning-time hygiene.
fn recorded_by_another_skill(ctx: &Ctx<'_>, skill_id: &str, dir: &Path) -> bool {
    if !ctx.fs.exists(&ctx.layout.skills_dir()) {
        return false;
    }
    let Ok(entries) = ctx.fs.read_dir(&ctx.layout.skills_dir()) else {
        return false;
    };
    // Records store CANONICALIZED paths (adopt-in-place canonicalizes its source); the candidate
    // may arrive through a symlinked root — and either side may be absent on disk. Raw equality
    // first, then the parent-resolved canonical forms.
    let canonical = canonical_or_parent(dir);
    for entry in entries {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if id.starts_with('.') || !entry.is_dir() || id == skill_id {
            continue;
        }
        let Ok(sid) = crate::id::SkillId::parse(id) else {
            continue;
        };
        let Ok(Some(map)) = crate::doc::read_map(ctx.fs, &ctx.layout.published(&sid).map) else {
            continue;
        };
        if map.placements.iter().any(|p| {
            let recorded = Path::new(p);
            recorded == dir
                || canonical
                    .as_deref()
                    .is_some_and(|c| canonical_or_parent(recorded).is_some_and(|r| r == c))
        }) {
            return true;
        }
    }
    false
}

/// Record ADOPTION RESERVATIONS on a freshly reconciled map: every never-materialized placement
/// whose dir already exists under the sanitized display `name` with content digest-equal to
/// `digest` — and which no OTHER tracked skill's record owns (the same guard the plan's adopt
/// choice applies) — gets that digest into `pre_existing_sha` — the durable adoption record
/// ([`prior_dir`] and [`classic_plan`] reuse it across plans; the sticky prior-bytes semantics
/// ride it). No bytes move here; `materialized_sha` stays untouched.
pub(crate) fn record_adoptions(
    ctx: &Ctx<'_>,
    map: &mut PlacementMap,
    skill_id: &str,
    name: &str,
    digest: &[u8; 32],
) {
    let Some(sanitized) = topos_harness::sanitize_skill_dir(name) else {
        return;
    };
    let probe = digest_probe(*digest);
    for (dir, st) in map.placements.iter().zip(map.placement_state.iter_mut()) {
        if st.materialized_sha.is_some() {
            continue;
        }
        let p = Path::new(dir);
        if p.file_name().and_then(|l| l.to_str()) == Some(sanitized.as_str())
            && topos_harness::dir_taken(p)
            && !recorded_by_another_skill(ctx, skill_id, p)
            && probe(p)
        {
            st.pre_existing_sha = Some(topos_core::digest::to_hex(digest));
        }
    }
}

/// The never-clobber ownership predicate: a dir counts as this skill's own iff the record names it
/// AND topos actually materialized bytes there (a recorded-but-never-placed reservation that someone
/// else has since occupied is NOT ours to overwrite).
fn owned_predicate(prior: Option<&PlacementMap>) -> impl Fn(&Path) -> bool + '_ {
    move |p: &Path| {
        prior.is_some_and(|map| {
            map.placements
                .iter()
                .zip(&map.placement_state)
                .any(|(dir, st)| Path::new(dir) == p && st.materialized_sha.is_some())
        })
    }
}

/// Reconcile the durable record with a fresh plan: every prior placement is KEPT (its dir and state
/// verbatim — a placement leaves the record only through an explicit verb), and every planned target
/// the record does not yet hold is APPENDED never-materialized. Returns the next map.
pub(crate) fn reconcile_map(prior: &PlacementMap, plan: &PlacementPlan) -> PlacementMap {
    let mut next = prior.clone();
    for t in &plan.targets {
        let dir = t.dir.to_string_lossy().into_owned();
        if next.placements.contains(&dir) {
            continue;
        }
        // A stale RESERVATION for the same (kind, agent) key — recorded, never materialized, and
        // re-chosen because its dir got occupied — is REPLACED in place (a reservation holds no
        // bytes, so nothing freezes); everything else appends. The slot's state resets with the
        // dir: the old reservation's `pre_existing_sha` described the OLD dir's occupant and must
        // never migrate to the new one.
        if let Some(i) = next
            .placements
            .iter()
            .zip(&next.placement_state)
            .position(|(_, st)| {
                st.kind == t.kind
                    && st.agent.as_deref() == t.agent.as_deref()
                    && st.materialized_sha.is_none()
            })
        {
            next.placements[i] = dir;
            next.placement_state[i] = PlacementState {
                kind: t.kind,
                agent: t.agent.clone(),
                materialized_sha: None,
                pre_existing_sha: None,
                swap_capability: SwapCapability::Unsupported,
            };
            continue;
        }
        next.placements.push(dir);
        next.placement_state.push(PlacementState {
            kind: t.kind,
            agent: t.agent.clone(),
            materialized_sha: None,
            pre_existing_sha: None,
            swap_capability: SwapCapability::Unsupported,
        });
    }
    next
}

/// The indices of `map`'s placements the CURRENT plan manages — the apply set. A recorded placement
/// outside the plan (a lost detection, an excluded agent whose clean has not run) is skipped: frozen
/// in place, never written, never deleted.
pub(crate) fn managed_indices(map: &PlacementMap, plan: &PlacementPlan) -> Vec<usize> {
    map.placements
        .iter()
        .enumerate()
        .filter(|(_, dir)| plan.targets.iter().any(|t| t.dir == Path::new(dir)))
        .map(|(i, _)| i)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The multi-placement work-tree scan — draft-anywhere classification.
// ---------------------------------------------------------------------------------------------

/// One placement's scan outcome, against ITS OWN recorded materialized sha.
pub(crate) enum ScanStatus {
    /// The dir does not exist (or is a dangling symlink).
    Absent,
    /// Bytes match the recorded sha — no local edits in this copy. Carries ONLY the `bundle_digest`
    /// (which equals the recorded sha): the stat cache may have proven this without reading a byte,
    /// so there is no `ScannedBundle` to hand out. A consumer that needs the working bytes of a clean
    /// copy re-scans the dir (the cold stale-replica / merge-escape paths do exactly that).
    Clean { digest: [u8; 32] },
    /// Bytes differ from the recorded sha — a local edit in this copy. ALWAYS carries the full scanned
    /// bundle (bytes), because every `Modified` consumer snapshots or commits those exact bytes.
    Modified { scanned: ScannedBundle },
    /// The record says topos never wrote here, yet the dir holds content — not ours; never scanned
    /// into drafts, never overwritten.
    Foreign,
    /// The dir exists but cannot be scanned safely — fail closed, never overwrite it.
    Unscannable,
}

/// One placement's scan row.
pub(crate) struct PlacementScan {
    pub idx: usize,
    pub dir: PathBuf,
    pub status: ScanStatus,
}

/// Scan every recorded placement against its per-placement materialized sha. The caller classifies
/// (see [`crate::ops::sync_engine::compute_work`]) or snapshots (reset / withdraw) from these rows.
///
/// The routine drift verdict is accelerated by the stat cache ([`crate::stat_cache`]) — a clean copy
/// is confirmed by `(mtime_ns, ctime_ns, size)` rather than a re-hash — unless `TOPOS_NO_STAT_CACHE=1`
/// disables it. The verdict is byte-for-byte identical either way (the cache only spares reads).
pub(crate) fn scan_placements(
    ctx: &Ctx<'_>,
    map: &PlacementMap,
) -> Result<Vec<PlacementScan>, ClientError> {
    scan_placements_cached(ctx, map, stat_cache::enabled_from_env())
}

/// The cache-mode-explicit core of [`scan_placements`] — the equivalence tests drive both modes here
/// without touching process-global env.
pub(crate) fn scan_placements_cached(
    ctx: &Ctx<'_>,
    map: &PlacementMap,
    cache_on: bool,
) -> Result<Vec<PlacementScan>, ClientError> {
    let mut cache = if cache_on {
        stat_cache::load(ctx.fs, &ctx.layout)
    } else {
        stat_cache::StatCache::default()
    };
    let original = cache.clone();
    // The racy-clean reference: when the cache was last persisted. Read BEFORE this scan writes it,
    // so a file touched at/after the last write is re-hashed rather than trusted.
    let racy_ref = cache_on
        .then(|| stat_cache::last_written_ns(&ctx.layout))
        .flatten();

    let mut out = Vec::with_capacity(map.placements.len());
    for (idx, (placement, state)) in map.placements.iter().zip(&map.placement_state).enumerate() {
        let dir = PathBuf::from(placement);
        let status = scan_one(ctx, &dir, state, cache_on.then_some(&mut cache), racy_ref)?;
        out.push(PlacementScan { idx, dir, status });
    }

    // Persist the refreshed cache only when it moved — best-effort, never a scan blocker.
    if cache_on && cache != original {
        let _ = stat_cache::store(ctx.fs, &ctx.layout, &cache);
    }
    Ok(out)
}

fn scan_one(
    ctx: &Ctx<'_>,
    dir: &Path,
    state: &PlacementState,
    cache: Option<&mut stat_cache::StatCache>,
    racy_ref: Option<i64>,
) -> Result<ScanStatus, ClientError> {
    match ctx.fs.path_kind(dir)? {
        None => return Ok(ScanStatus::Absent),
        // A dangling symlink (its target is gone — e.g. a crash in the rename-dance absent window)
        // is effectively ABSENT: the next apply first-installs into the resolved target and recovers.
        Some(crate::fs_seam::PathKind::Symlink) if std::fs::canonicalize(dir).is_err() => {
            return Ok(ScanStatus::Absent);
        }
        _ => {}
    }

    // A dir the record says we never wrote (no recorded sha) is FOREIGN when scannable, else
    // UNSCANNABLE — decided by a full scan (rare; never cache-accelerated, no digest to compare).
    let Some(recorded) = state.materialized_sha.as_deref() else {
        return Ok(match scan::scan(dir) {
            Ok(_) => ScanStatus::Foreign,
            Err(_) => ScanStatus::Unscannable,
        });
    };

    // The FAST path: prove clean-vs-modified from the cached per-file shas, reading only changed
    // files. A cached-walk failure (or the cache disabled) falls through to a full scan below.
    if let Some(cache) = cache {
        let key = dir.to_string_lossy().into_owned();
        let prev = cache
            .placements
            .get(&key)
            .and_then(|b| b.usable_rows(recorded).cloned());
        if let Ok(drift) = scan::drift_digest(dir, prev.as_ref(), racy_ref) {
            let clean = topos_core::digest::to_hex(&drift.bundle_digest) == *recorded;
            let digest = drift.bundle_digest;
            // Refresh the bucket to the freshly observed rows (basis = the recorded sha these rows
            // were compared against); bump the generation when anything moved.
            update_bucket(
                cache.placements.entry(key).or_default(),
                recorded,
                drift.files,
            );
            return Ok(if clean {
                ScanStatus::Clean { digest }
            } else {
                // A draft: the byte-shipping consumers need the exact bytes, so a Modified status
                // always carries the FULL scan (never the digest-only fast path).
                match scan::scan(dir) {
                    Ok(scanned) => ScanStatus::Modified { scanned },
                    Err(_) => ScanStatus::Unscannable,
                }
            });
        }
        // The cached walk hit a hazard (or a read error) — fall through to the full scan, which
        // classifies it identically (Unscannable on the same failure); no empty bucket is left.
    }

    let Ok(scanned) = scan::scan(dir) else {
        return Ok(ScanStatus::Unscannable);
    };
    Ok(
        if topos_core::digest::to_hex(&scanned.bundle_digest) == *recorded {
            ScanStatus::Clean {
                digest: scanned.bundle_digest,
            }
        } else {
            ScanStatus::Modified { scanned }
        },
    )
}

/// Replace a placement bucket's rows with the freshly observed set, tagging them with the recorded
/// sha they were compared against and bumping the generation whenever the basis or rows moved (the
/// visible marker that a swap invalidation, or an edit, was absorbed).
fn update_bucket(
    bucket: &mut stat_cache::PlacementBucket,
    recorded: &str,
    files: std::collections::BTreeMap<String, stat_cache::FileStat>,
) {
    let changed = bucket.basis.as_deref() != Some(recorded) || bucket.files != files;
    if changed {
        bucket.generation = bucket.generation.saturating_add(1);
        bucket.basis = Some(recorded.to_owned());
        bucket.files = files;
    }
}

/// The distinct MODIFIED copies among the scans, deduped by digest (several byte-identical edited
/// copies are ONE logical draft). Returns `(index of the first copy per distinct digest, digest)`.
pub(crate) fn distinct_modified(scans: &[PlacementScan]) -> Vec<(usize, String)> {
    let mut seen: Vec<(usize, String)> = Vec::new();
    for s in scans {
        if let ScanStatus::Modified { scanned } = &s.status {
            let hex = topos_core::digest::to_hex(&scanned.bundle_digest);
            if !seen.iter().any(|(_, d)| *d == hex) {
                seen.push((s.idx, hex));
            }
        }
    }
    seen
}

/// The dir the single-work-tree surfaces (diff / publish / merge) read: the ONE modified copy when
/// exactly one exists (the draft), else the first materialized placement. MORE than one distinct
/// modified copy is the typed freeze — nothing to read until reconciled.
///
/// # Errors
/// [`ClientError::PlacementsDiverged`] on several distinct modified copies;
/// [`ClientError::Corrupt`] when the map records no placement at all.
pub(crate) fn work_tree_dir(
    ctx: &Ctx<'_>,
    skill_name: &str,
    map: &PlacementMap,
) -> Result<PathBuf, ClientError> {
    let scans = scan_placements(ctx, map)?;
    let modified = distinct_modified(&scans);
    if modified.len() > 1 {
        return Err(placements_diverged(skill_name, &scans));
    }
    if let Some((idx, _)) = modified.first() {
        return Ok(scans[*idx].dir.clone());
    }
    // No draft: the first placement that holds our bytes, else the first recorded placement (the
    // classic read surface for an absent working tree — callers report their own absence).
    let first_clean = scans
        .iter()
        .find(|s| matches!(s.status, ScanStatus::Clean { .. }))
        .map(|s| s.dir.clone());
    first_clean
        .or_else(|| map.placements.first().map(PathBuf::from))
        .ok_or_else(|| ClientError::Corrupt("placement map has no placement".into()))
}

/// The typed multi-draft freeze, with its per-path disclosure (every modified copy named).
pub(crate) fn placements_diverged(skill_name: &str, scans: &[PlacementScan]) -> ClientError {
    let paths: Vec<String> = scans
        .iter()
        .filter(|s| matches!(s.status, ScanStatus::Modified { .. }))
        .map(|s| s.dir.display().to_string())
        .collect();
    ClientError::PlacementsDiverged {
        skill: skill_name.to_owned(),
        paths,
    }
}

/// The workspace ADDRESS slug for `workspace_id` (the collision namespace `choose_skill_dir`
/// suffixes with), from the enrolled memberships — best-effort (`None` offline / unenrolled).
pub(crate) fn workspace_slug(ctx: &Ctx<'_>, workspace_id: Option<&str>) -> Option<String> {
    let ws = workspace_id?;
    crate::enroll::read_user(ctx.fs, &ctx.layout)
        .ok()
        .flatten()
        .and_then(|u| u.membership(ws).map(|m| m.name.clone()))
}

/// The plan for an ALREADY-TRACKED skill: naming from its lock, scope + workspace slug from the
/// follow-state. The one entry every re-plan site (sync / reset / go-back / the verbs) calls, so the
/// target set is computed identically everywhere. The engine plans breadth for FOLLOWED skills only
/// — a purely-local skill (adopted in place, never followed) keeps its recorded placement as-is: its
/// dir is the user's own working location, and nothing distributes it.
pub(crate) fn plan_for_skill(
    ctx: &Ctx<'_>,
    skill_id: &str,
    lock: &Lock,
    prior: &PlacementMap,
) -> PlacementPlan {
    let ws = crate::ops::followed_workspace(ctx, skill_id);
    if ws.is_none() {
        return classic_plan(
            ctx,
            skill_id,
            PlacementNaming {
                name: Some(&lock.name),
                workspace_slug: None,
            },
            Some(prior),
            None,
        );
    }
    let (agents, excluded) = scope_of(ctx, skill_id);
    let slug = workspace_slug(ctx, ws.as_deref());
    // No adopt probe here: a tracked skill's adoption continuity rides the record — the baseline's
    // adoption reservation (`pre_existing_sha`) keeps [`prior_dir`] answering the adopted dir.
    plan_targets(
        ctx,
        skill_id,
        PlacementNaming {
            name: Some(&lock.name),
            workspace_slug: slug.as_deref(),
        },
        AgentScope {
            agents: &agents,
            excluded: &excluded,
        },
        Some(prior),
        None,
    )
}

/// Validate `--agent` slugs against the baked registry: unknown slugs refuse, naming the valid ones.
/// `'*'` is the caller's sentinel (handled before validation). Returns the DETECTED subset's
/// complement as notes fodder — the caller discloses "known but not detected here".
///
/// # Errors
/// [`ClientError::InvalidArgument`] naming every valid slug on an unknown one.
pub(crate) fn validate_agent_slugs(
    ctx: &Ctx<'_>,
    slugs: &[String],
) -> Result<Vec<String>, ClientError> {
    let known = registry::known_harnesses();
    for s in slugs {
        if !known.iter().any(|h| h.slug == s.as_str()) {
            return Err(ClientError::InvalidArgument(format!(
                "'{s}' is not a known agent — valid agents: {}",
                known.iter().map(|h| h.slug).collect::<Vec<_>>().join(", ")
            )));
        }
    }
    let detected: Vec<&str> = match &ctx.roots {
        Some(roots) => registry::detected_harnesses(&roots.home, roots.cwd.as_deref())
            .into_iter()
            .map(|h| h.slug)
            .collect(),
        None => Vec::new(),
    };
    Ok(slugs
        .iter()
        .filter(|s| !detected.contains(&s.as_str()))
        .cloned()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-entry never-materialized map (a reservation) whose slot carries a recorded adoption.
    fn reservation_map(dir: &str, pre_existing: Option<&str>) -> PlacementMap {
        PlacementMap {
            schema_version: topos_types::PLACEMENT_MAP_SCHEMA_VERSION,
            placements: vec![dir.to_owned()],
            applied_commit: "0".repeat(64),
            materialized_sha: "0".repeat(64),
            pre_existing_sha: None,
            swap_capability: SwapCapability::Unsupported,
            placement_state: vec![PlacementState {
                kind: PlacementKind::Native,
                agent: Some("claude-code".to_owned()),
                materialized_sha: None,
                pre_existing_sha: pre_existing.map(str::to_owned),
                swap_capability: SwapCapability::AtomicExchange,
            }],
            harness: None,
            harness_layer: None,
            harness_slug: None,
        }
    }

    /// Replacing a stale reservation's dir RESETS the slot's state (kind/agent kept): the old
    /// adoption's `pre_existing_sha` described the OLD occupant and must never migrate to the new
    /// dir, and the cached swap capability was probed against the old parent.
    #[test]
    fn a_replaced_stale_reservation_resets_the_slot_state() {
        let prior = reservation_map("/skills/deploy", Some(&"a".repeat(64)));
        let plan = PlacementPlan {
            targets: vec![PlannedTarget {
                dir: PathBuf::from("/skills/deploy-acme"),
                kind: PlacementKind::Native,
                agent: Some("claude-code".to_owned()),
            }],
            shared_covers: Vec::new(),
        };
        let next = reconcile_map(&prior, &plan);
        assert_eq!(next.placements, vec!["/skills/deploy-acme".to_owned()]);
        let st = &next.placement_state[0];
        assert_eq!(st.kind, PlacementKind::Native);
        assert_eq!(st.agent.as_deref(), Some("claude-code"));
        assert!(st.materialized_sha.is_none());
        assert!(st.pre_existing_sha.is_none(), "no adoption record migrates");
        assert_eq!(st.swap_capability, SwapCapability::Unsupported);
    }

    /// The same-dir plan (a still-valid reservation) keeps the slot verbatim — the reset fires
    /// only on an actual replacement.
    #[test]
    fn an_unchanged_reservation_keeps_its_recorded_state() {
        let prior = reservation_map("/skills/deploy", Some(&"a".repeat(64)));
        let plan = PlacementPlan {
            targets: vec![PlannedTarget {
                dir: PathBuf::from("/skills/deploy"),
                kind: PlacementKind::Native,
                agent: Some("claude-code".to_owned()),
            }],
            shared_covers: Vec::new(),
        };
        let next = reconcile_map(&prior, &plan);
        assert_eq!(next.placements, vec!["/skills/deploy".to_owned()]);
        assert_eq!(
            next.placement_state[0].pre_existing_sha.as_deref(),
            Some("a".repeat(64).as_str())
        );
    }
}
