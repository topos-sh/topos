//! `list [<skill>] [--footprint]` — inventory this machine. Populates the **tracked** bucket (every
//! skill with a local sidecar record) and, once enrolled, the **followed** bucket (the tracked subset
//! `follows.json` says is following its workspace `current`) plus a TTY enrollment header (workspace,
//! plane, auto-update-hook state) — the one-command answer to "am I enrolled, what am I following, is the
//! hook armed". `published_by_you` stays empty: the client keeps no durable record of its own settled
//! publishes (the op-WAL is deleted once an op settles; `lock.json` records no author), so that bucket
//! honestly waits for the plane-side `log --team` read. `untracked` needs harness discovery wiring and
//! renders empty. `--footprint` reports every topos-owned path outside skill dirs: the `~/.topos/` tree
//! plus any harness config the auto-update hook lives in (disclosed, never deleted).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use topos_core::digest::to_hex;
use topos_harness::registry::{self, SkillScope};
use topos_types::persisted::{Lock, PlacementMap};
use topos_types::requests::WireSkillIndexEntry;
use topos_types::results::{
    BucketTruncation, DetachCause, ListData, RemoteFollowState, RemoteSkillEntry, SkillEntry,
    SkillStatus, UntrackedEntry,
};

use crate::ctx::Ctx;
use crate::enroll::{self, FollowEntry, FollowModeDoc};
use crate::error::ClientError;
use crate::plane::{CatalogSource, PlaneError};
use crate::scan;
use crate::sidecar;
use crate::sync_status::{self, DeliveredSkill};
use crate::{doc, scan::ScannedBundle};

/// The filesystem roots `list` probes for **untracked** skills: the user home (every harness's global
/// skill dir resolves under it) and, optionally, the current project dir (for repo-scoped skills). Passing
/// `None` to [`list`] is `--tracked` — discovery is skipped entirely.
#[derive(Debug, Clone)]
pub(crate) struct DiscoveryRoots {
    pub home: PathBuf,
    pub cwd: Option<PathBuf>,
}

/// A `list` run's typed result: the schema-pinned envelope payload plus the TTY-only enrollment
/// disclosure. `ListData` is PINNED (its buckets carry `SkillEntry` rows only), so the enrollment header
/// and the per-row follow annotations ride alongside for the TTY renderer — mirroring how `pull`'s
/// warnings ride outside `PullData`.
#[derive(Debug)]
pub(crate) struct ListOutcome {
    pub data: ListData,
    /// `Some` iff enrolled (`instance.json` present — the same presence rule `load_enrollment` uses).
    pub enrollment: Option<ListEnrollment>,
    /// Per-workspace `--remote` catalog-read failures (one stable-shape line each) — the SAME degradation
    /// shape `pull` uses: a transport fault reading one workspace's catalog skips it with a warning rather
    /// than failing the whole `list`. Empty on the local-only path. Rides the `--json` envelope's
    /// `warnings` + the TTY, outside the pinned `ListData`.
    pub warnings: Vec<String>,
}

/// The `--remote` scope: what `list` needs to read the followed workspaces' catalogs and annotate each
/// entry with local follow-state. `pub(crate)` — built by the composition root (real `ureq` transport
/// holding the per-workspace credential map + the memberships from `user.json`) and by the test (a fake
/// transport). Present only under `--remote`; `None` is the local-only path.
pub(crate) struct RemoteScope<'a> {
    /// The catalog transport (`GET /v1/workspaces/{ws}/skills`, presenting the workspace's Bearer credential
    /// looked up in its own credential map).
    pub catalog: &'a dyn CatalogSource,
    /// Every workspace this install has joined, as `(workspace_id, display_label)` (from `user.json`) — the
    /// catalog targets.
    pub memberships: Vec<(String, String)>,
    /// The global `--workspace` filter, pre-canonicalized to an id (narrows to one joined workspace);
    /// `None` = every joined one.
    pub only: Option<String>,
}

impl RemoteScope<'_> {
    /// The workspaces to read the catalog from: the memberships, narrowed by the `--workspace` filter.
    fn target_workspaces(&self) -> Vec<(&str, &str)> {
        self.memberships
            .iter()
            .filter(|(id, _)| self.only.as_deref().is_none_or(|w| w == id))
            .map(|(id, label)| (id.as_str(), label.as_str()))
            .collect()
    }
}

/// The enrolled-state disclosure for the TTY header + row annotations.
#[derive(Debug)]
pub(crate) struct ListEnrollment {
    /// The joined workspaces as `(workspace_id, display_label)` in membership order — the TTY groups the
    /// tracked rows by their `workspace_id` and names each group by its label (falling back to the raw id).
    pub workspace_labels: Vec<(String, String)>,
    /// The enrolled plane's base URL.
    pub base_url: String,
    /// Whether the harness session-start auto-update hook is currently installed (read from the adapter's
    /// managed-entry disclosure — it names its config path only while the managed entry is present).
    pub hook_active: bool,
    /// One entry per `data.tracked` row, same order: the follow-state note, or `None` for a purely
    /// local (never-followed) skill.
    pub notes: Vec<Option<FollowNote>>,
}

/// One tracked row's follow state, from `follows.json`.
#[derive(Debug)]
pub(crate) struct FollowNote {
    /// `"auto"` / `"confirm-each"`.
    pub mode: &'static str,
    /// `false` = the entry is retained but unfollowed (`topos follow <skill>` resumes it).
    pub following: bool,
}

/// A `list` invocation's row filter: positional NAMES, `--skill` selectors (both matched by skill
/// name), and `--channel` selectors (matched offline against each skill's cached `via` channels). A
/// single positional name keeps the classic narrowing (proposals annotation + the exactly-one gate);
/// any richer form resolves ALL-OR-NONE (an unmatched name refuses the whole invocation) and filters
/// the tracked rows to the union of what matched.
#[derive(Debug, Default, Clone)]
pub(crate) struct ListFilter {
    pub names: Vec<String>,
    pub channels: Vec<String>,
    pub skills: Vec<String>,
}

impl ListFilter {
    /// A single positional name with no selectors — the classic `list <skill>` narrowing.
    fn single_name(&self) -> Option<&str> {
        if self.channels.is_empty() && self.skills.is_empty() && self.names.len() == 1 {
            Some(self.names[0].as_str())
        } else {
            None
        }
    }

    /// Any filter at all — narrows the view (suppresses untracked discovery + the remote catalog, the
    /// same way the classic single name does).
    fn narrows(&self) -> bool {
        !(self.names.is_empty() && self.channels.is_empty() && self.skills.is_empty())
    }
}

/// Inventory the tracked skills, optionally narrowed to one name and/or with the footprint, and — under
/// `--remote` ([`RemoteScope`] present) — the followed workspaces' catalogs annotated with local
/// follow-state (a per-workspace transport fault DEGRADES to a warning, never failing the whole `list`).
///
/// The classic single-name entry point — a thin wrapper over [`list_with`] so the inline tests and the
/// feature-gated e2e rig keep the `Option<&str>` shape. Production (`app.rs`) calls [`list_with`] with the
/// full filter, so this shim is compiled only for tests / the `test-fixtures` facade.
///
/// # Errors
/// [`ClientError::NoSuchSkill`] / [`ClientError::AmbiguousName`] when a name filter does not resolve to
/// exactly one skill; otherwise a read failure.
#[cfg(any(test, feature = "test-fixtures"))]
pub(crate) fn list(
    ctx: &Ctx<'_>,
    skill: Option<&str>,
    want_footprint: bool,
    discover: Option<DiscoveryRoots>,
    remote: Option<RemoteScope<'_>>,
) -> Result<ListOutcome, ClientError> {
    let filter = ListFilter {
        names: skill.map(|s| vec![s.to_owned()]).unwrap_or_default(),
        ..ListFilter::default()
    };
    list_with(
        ctx,
        &filter,
        want_footprint,
        discover,
        remote,
        crate::ops::RowPage::unlimited(),
    )
}

/// Inventory the tracked skills under a full [`ListFilter`] (positional names + `--channel`/`--skill`
/// selectors), the footprint, and the optional `--remote` catalog — row-capped by `page` (the
/// `--json` default page / the `--limit`/`--offset` flags), applied PER BUCKET with a
/// [`BucketTruncation`] marker per capped bucket.
///
/// # Errors
/// [`ClientError::NoSuchSkill`] when a name selector matches no tracked skill; the uniform not-found when
/// a `--channel` selector matches no delivered skill; [`ClientError::AmbiguousName`] for the classic
/// single-name over-match; otherwise a read failure.
pub(crate) fn list_with(
    ctx: &Ctx<'_>,
    filter: &ListFilter,
    want_footprint: bool,
    discover: Option<DiscoveryRoots>,
    remote: Option<RemoteScope<'_>>,
    page: super::RowPage,
) -> Result<ListOutcome, ClientError> {
    // The follow-state is the ONE source for the per-skill workspace provenance, the followed bucket, and
    // the TTY notes — read it once here (absent ⇒ empty, e.g. unenrolled or a membership-only door). We
    // deliberately do NOT consult `ctx.follow`: `list` already keys its followed bucket + notes off this
    // file read, so the per-entry `workspace_id` shares that single authority (they can only agree).
    let follows = enroll::read_follows(ctx.fs, &ctx.layout)?
        .map(|f| f.follows)
        .unwrap_or_default();
    // The offline signals the SOURCE/STATUS/CAUSE columns read: the stored device credential (its
    // absence means every followed workspace is signed out here) and the membership labels (the
    // friendly source name for a followed skill). Both best-effort — absence just narrows what a
    // column can say.
    let signed_in = enroll::read_credentials(ctx.fs, &ctx.layout)?.is_some();
    let labels: HashMap<String, String> = enroll::read_user(ctx.fs, &ctx.layout)?
        .map(|u| {
            u.workspaces
                .into_iter()
                .map(|m| (m.workspace_id, m.display_name))
                .collect()
        })
        .unwrap_or_default();
    // The last reconcile's per-skill delivery cache — the offline source of the `behind` status and the
    // `removed-upstream` cause, plus each skill's `via` channels for a `--channel` filter. Best-effort
    // (absent ⇒ no cache signal; `list` never wedges on the advisory doc).
    let sync = sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();

    // Carry the stable skill id alongside each entry — the proposals read route and the follow-state
    // are keyed by id, not name.
    let mut tracked: Vec<(String, SkillEntry)> = Vec::new();
    for entry in ctx.fs.read_dir(&ctx.layout.skills_dir())? {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Skip the transient staging dirs (and anything else hidden); a skill id never starts with '.'.
        if id.starts_with('.') || !entry.is_dir() {
            continue;
        }
        // A dir name outside the validated id charset was never minted by topos — not a tracked skill.
        let Ok(id) = crate::id::SkillId::parse(id) else {
            continue;
        };
        let paths = ctx.layout.published(&id);
        let Some(lock): Option<Lock> = doc::read_doc(ctx.fs, &paths.lock)? else {
            continue;
        };
        let draft = is_draft(ctx, &paths.map, &lock)?;
        let id_str = id.into_string();
        // The skill's follow entry (a retained-but-paused entry still carries its workspace); `None`
        // for a purely local, never-followed `add`'d skill.
        let follow_entry = follows.iter().find(|f| f.skill_id == id_str);
        let workspace_id = follow_entry.map(|f| f.workspace_id.clone());
        // The recorded remote import origin (best-effort — absence means no upstream).
        let origin_host = doc::read_doc::<crate::ops::add::OriginDoc>(ctx.fs, &paths.origin)
            .ok()
            .flatten()
            .and_then(|o| origin_host(&o.origin.source));
        // The skill's last-delivery cache entry (served version + withdrawn flag) — offline `behind` +
        // `removed-upstream`.
        let delivered = workspace_id
            .as_deref()
            .and_then(|w| sync.workspaces.get(w))
            .and_then(|ws| ws.delivered.get(&id_str));
        let (source, status, cause) = if crate::ops::builtin::is_builtin(&id_str) {
            // The built-in skill: shipped by the CLI, force-synced to the binary. A hand edit shows
            // `draft` honestly until the next sweep overwrites it (snapshot-first).
            (
                Some("built-in".to_owned()),
                Some(if draft {
                    SkillStatus::Draft
                } else {
                    SkillStatus::Current
                }),
                None,
            )
        } else {
            derive_columns(
                follow_entry,
                draft,
                origin_host,
                workspace_id.as_deref().and_then(|w| labels.get(w)),
                workspace_id.is_none() || signed_in,
                delivered,
                &lock.base_commit,
            )
        };
        tracked.push((
            id_str,
            SkillEntry {
                skill: lock.name,
                workspace_id,
                version_id: lock.base_commit,
                bundle_digest: lock.bundle_digest,
                draft,
                pending_proposals: Vec::new(),
                source,
                status,
                cause,
            },
        ));
    }
    // Deterministic order (name, then version).
    tracked.sort_by(|a, b| {
        a.1.skill
            .cmp(&b.1.skill)
            .then_with(|| a.1.version_id.cmp(&b.1.version_id))
    });

    let narrowed = filter.narrows();
    if let Some(want) = filter.single_name() {
        // The classic `list <skill>` narrowing: exactly-one gate + the OPEN-proposals annotation.
        let count = tracked.iter().filter(|(_, e)| e.skill == want).count();
        match count {
            0 => {
                return Err(ClientError::NoSuchSkill {
                    name: want.to_owned(),
                });
            }
            1 => {
                tracked.retain(|(_, e)| e.skill == want);
                // For the narrowed skill, annotate its OPEN proposals as `<skill>@<hash>` (best-effort —
                // a plane-read failure / a local-only skill leaves it empty; the bare `list` skips this to
                // avoid a network GET per skill).
                if let Some((id, entry)) = tracked.first_mut()
                    && let Ok(handles) = ctx.plane.list_open_proposals(id)
                {
                    entry.pending_proposals = handles
                        .iter()
                        .map(|h| format!("{}@{}", entry.skill, to_hex(h)))
                        .collect();
                }
            }
            count => {
                return Err(ClientError::AmbiguousName {
                    name: want.to_owned(),
                    count,
                });
            }
        }
    } else if narrowed {
        // The multi-name / `--skill` / `--channel` filter: resolve ALL-OR-NONE, keep the union.
        tracked = apply_filter(tracked, filter, &sync)?;
    }

    // The enrolled-state disclosure + the followed bucket, from the same docs the pull engine reads.
    // `instance.json` present = enrolled (its presence is what `follow` writes); `follows.json` may be
    // absent (a membership-only enrollment). A followed skill always has a sidecar record (`follow` lays
    // the first-receive baseline), so the followed bucket is the tracked subset its ids select; a
    // follows entry with no local record (a foreign/partial state) is simply not listable yet.
    let mut enrollment = match enroll::read_instance(ctx.fs, &ctx.layout)? {
        None => None,
        Some(instance) => {
            let notes: Vec<Option<FollowNote>> = tracked
                .iter()
                .map(|(id, _)| {
                    follows
                        .iter()
                        .find(|f| f.skill_id == *id)
                        .map(|f| FollowNote {
                            mode: match f.mode {
                                FollowModeDoc::Auto => "auto",
                                FollowModeDoc::ConfirmEach => "confirm-each",
                            },
                            following: f.following,
                        })
                })
                .collect();
            // The per-workspace display names now live per-membership in user.json (instance.json is the
            // plane record only). Carry every membership's `(id, label)` so the TTY groups the tracked rows
            // by workspace and names each group — one install can follow skills across several workspaces.
            let workspace_labels = enroll::read_user(ctx.fs, &ctx.layout)?
                .map(|u| {
                    u.workspaces
                        .into_iter()
                        .map(|m| {
                            let label = m.display_name;
                            (m.workspace_id, label)
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(ListEnrollment {
                workspace_labels,
                base_url: instance.base_url,
                hook_active: ctx.harness.trigger_present(),
                notes,
            })
        }
    };
    let followed: Vec<SkillEntry> = match &enrollment {
        Some(e) => tracked
            .iter()
            .zip(&e.notes)
            .filter(|(_, n)| n.as_ref().is_some_and(|n| n.following))
            .map(|((_, entry), _)| entry.clone())
            .collect(),
        None => Vec::new(),
    };
    // The local applied version per tracked skill, keyed by id — the cheap `Following`/`FollowingBehind`
    // discriminant for the `--remote` merge below (the sidecar `lock`'s `base_commit` is the version this
    // install is on). Captured before `tracked` is flattened to `SkillEntry` rows (which drop the id key).
    let local_versions: HashMap<String, String> = tracked
        .iter()
        .map(|(id, e)| (id.clone(), e.version_id.clone()))
        .collect();
    let tracked: Vec<SkillEntry> = tracked.into_iter().map(|(_, e)| e).collect();

    let footprint = if want_footprint {
        // The `~/.topos/` walk PLUS any harness config path topos holds a managed entry in (disclosed,
        // never deleted) — every topos-owned path outside skill dirs.
        let mut paths = sidecar::footprint(ctx.fs, &ctx.layout)?;
        paths.extend(
            ctx.harness
                .uninstall_footprint()
                .iter()
                .map(|p| p.to_string_lossy().into_owned()),
        );
        paths.sort();
        Some(paths)
    } else {
        None
    };

    // Discover untracked skills across the baked harness registry — only on a bare sweep (any filter
    // narrows to tracked rows) and only when not `--tracked`. Dedups against every tracked placement so an
    // adopted/followed skill never shows up as "untracked".
    let untracked = if let (Some(roots), false) = (&discover, narrowed) {
        discover_untracked(ctx, roots)?
    } else {
        Vec::new()
    };

    // The `--remote` catalog: for each followed workspace, a catalog read merged with the
    // local follow-state. A per-workspace transport fault degrades to a warning (never fails the `list`).
    let mut warnings: Vec<String> = Vec::new();
    let remote_available = match (remote, narrowed) {
        // Bare-sweep only, mirroring untracked discovery above: the catalog is a browse of the WHOLE
        // workspace, so any filter skips it. This also keeps `local_versions` complete (captured after the
        // no-op narrowing), so the follow-state merge can never mislabel a followed skill the narrowing
        // dropped as `Following` when it is really `FollowingBehind`.
        (Some(scope), false) => build_remote(&scope, &follows, &local_versions, &mut warnings),
        (Some(_), true) => {
            warnings.push(
                "the remote catalog is listed only on a bare `topos list --remote`, not with a name/channel/skill filter — skipped".to_owned(),
            );
            Vec::new()
        }
        (None, _) => Vec::new(),
    };

    // The row page, applied LAST and PER BUCKET (after the remote merge, whose follow-state
    // discriminant needs the complete tracked set): each bucket independently skips `offset` rows
    // and emits up to `limit`, with one truncation marker per bucket that lost rows. The TTY's
    // per-row follow notes are index-aligned with `tracked`, so they slice under the SAME page —
    // alignment is preserved by construction. An inactive page keeps the exact prior shape.
    let mut followed = followed;
    let mut published_by_you: Vec<SkillEntry> = Vec::new();
    let mut tracked = tracked;
    let mut untracked = untracked;
    let mut remote_available = remote_available;
    let mut truncated: Vec<BucketTruncation> = Vec::new();
    if page.is_active() {
        let mut mark = |bucket: &str, (shown, total): (usize, usize)| {
            if shown < total {
                truncated.push(BucketTruncation {
                    bucket: bucket.to_owned(),
                    shown: shown as u64,
                    total: total as u64,
                });
            }
        };
        mark("followed", page.apply(&mut followed));
        mark("published_by_you", page.apply(&mut published_by_you));
        mark("tracked", page.apply(&mut tracked));
        if let Some(e) = &mut enrollment {
            page.apply(&mut e.notes);
        }
        mark("untracked", page.apply(&mut untracked));
        mark("remote_available", page.apply(&mut remote_available));
    }

    Ok(ListOutcome {
        data: ListData {
            followed,
            published_by_you,
            tracked,
            untracked,
            remote_available,
            footprint,
            truncated,
        },
        enrollment,
        warnings,
    })
}

/// Read each target workspace's catalog and merge every entry with the local follow-state.
/// A per-workspace signing or transport fault DEGRADES: the workspace is skipped with a stable-shape
/// warning line (the same isolation `pull`'s sweep uses), and the successfully-read workspaces still land.
/// The result is sorted deterministically by `(workspace_id, skill_id)`.
fn build_remote(
    scope: &RemoteScope<'_>,
    follows: &[FollowEntry],
    local_versions: &HashMap<String, String>,
    warnings: &mut Vec<String>,
) -> Vec<RemoteSkillEntry> {
    let mut out: Vec<RemoteSkillEntry> = Vec::new();
    for (ws_id, ws_label) in scope.target_workspaces() {
        // Read THIS workspace's catalog — authorized by the workspace's Bearer credential (catalog
        // visibility == workspace membership, resolved from the registry row). A workspace with no stored
        // credential degrades to the warning below like any other per-workspace fault.
        let index = match scope.catalog.fetch_catalog(ws_id) {
            Ok(index) => index,
            Err(e) => {
                warnings.push(format!(
                    "could not read the catalog for workspace {ws_label} ({}) — skipped",
                    catalog_err_label(&e)
                ));
                continue;
            }
        };
        for entry in &index.skills {
            out.push(RemoteSkillEntry {
                skill_id: entry.skill_id.clone(),
                workspace_id: ws_id.to_owned(),
                kind: entry.kind.clone(),
                display_name: entry.display_name.clone(),
                version_id: entry.version_id.clone(),
                bundle_digest: entry.bundle_digest.clone(),
                open_proposals: entry.open_proposals,
                state: merge_follow_state(entry, ws_id, follows, local_versions),
            });
        }
    }
    // Deterministic: workspace_id, then skill_id.
    out.sort_by(|a, b| {
        a.workspace_id
            .cmp(&b.workspace_id)
            .then_with(|| a.skill_id.cmp(&b.skill_id))
    });
    out
}

/// The local follow-state annotation for one catalog entry:
/// - **`Available`** — no `following == true` [`FollowEntry`] matches `(workspace_id, skill_id)`;
/// - **`Following`** — followed, and the local applied version matches the catalog `current` (OR the local
///   version can't be cheaply determined — we default a followed skill to `Following`, never wrongly
///   claiming it is behind);
/// - **`FollowingBehind`** — followed, but the local applied version differs from the catalog `current`
///   (the catalog has moved on — `pull` to advance).
fn merge_follow_state(
    entry: &WireSkillIndexEntry,
    workspace_id: &str,
    follows: &[FollowEntry],
    local_versions: &HashMap<String, String>,
) -> RemoteFollowState {
    let followed = follows
        .iter()
        .any(|f| f.skill_id == entry.skill_id && f.workspace_id == workspace_id && f.following);
    if !followed {
        return RemoteFollowState::Available;
    }
    match local_versions.get(&entry.skill_id) {
        Some(local) if *local != entry.version_id => RemoteFollowState::FollowingBehind,
        // Matches, or no cheap local version to compare — default to Following (never falsely "behind").
        _ => RemoteFollowState::Following,
    }
}

/// A short, leak-free label for a per-workspace catalog-read failure (rides a warning line).
fn catalog_err_label(e: &PlaneError) -> &'static str {
    match e {
        // The real transport maps 404 to an empty index, so `NotFound` here is only a defensive fallback.
        PlaneError::NotFound => "not authorized",
        PlaneError::Unreachable(_) => "plane unreachable",
        PlaneError::Unavailable(_) => "temporarily unavailable",
        PlaneError::Malformed(_) => "malformed response",
    }
}

/// Discover skills sitting in a known harness's skill dir (across the baked registry) that no tracked skill
/// already records — the `add`-able inventory. Dedups a physically-shared dir (e.g. `.agents/skills`) to
/// one row by canonical path. Real-fs (like the adapters' own `discover`), so a per-dir scan failure is
/// silently skipped, never an error. `pub(crate)` so `add <skill>` name resolution shares the SAME
/// discovered inventory `list` prints (one source of truth for what a name can resolve to).
pub(crate) fn discover_untracked(
    ctx: &Ctx<'_>,
    roots: &DiscoveryRoots,
) -> Result<Vec<UntrackedEntry>, ClientError> {
    let tracked = tracked_placement_paths(ctx)?;
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut out: Vec<UntrackedEntry> = Vec::new();
    for d in registry::discover_all(&roots.home, roots.cwd.as_deref()) {
        let canon = d.path.canonicalize().unwrap_or_else(|_| d.path.clone());
        if tracked.contains(&canon) {
            continue; // already adopted or followed — not "untracked"
        }
        if !seen.insert(canon) {
            continue; // one physical dir once (a dir shared across harnesses, e.g. .agents/skills)
        }
        let name = d
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| d.path.to_string_lossy().into_owned());
        out.push(UntrackedEntry {
            name,
            path: d.path.to_string_lossy().into_owned(),
            harness: d.harness_slug,
            harness_name: d.harness_name,
            adapter_supported: d.adapter_supported,
            scope: match d.scope {
                SkillScope::User => "user",
                SkillScope::Project => "project",
            }
            .to_owned(),
        });
    }
    // Deterministic order: name, then path.
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    Ok(out)
}

/// Every tracked skill's placement paths, canonicalized (a placement that no longer resolves on disk is
/// dropped — it can't shadow a real discovery). The same dedup key `add`'s `reject_already_tracked` uses.
fn tracked_placement_paths(ctx: &Ctx<'_>) -> Result<Vec<PathBuf>, ClientError> {
    let mut paths = Vec::new();
    for entry in ctx.fs.read_dir(&ctx.layout.skills_dir())? {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if id.starts_with('.') || !entry.is_dir() {
            continue;
        }
        let Ok(id) = crate::id::SkillId::parse(id) else {
            continue;
        };
        let Some(map): Option<PlacementMap> =
            doc::read_map(ctx.fs, &ctx.layout.published(&id).map)?
        else {
            continue;
        };
        for p in &map.placements {
            if let Ok(canon) = Path::new(p).canonicalize() {
                paths.push(canon);
            }
        }
    }
    Ok(paths)
}

/// A skill carries a draft iff ANY of its placements holds bytes hashing to a different
/// `bundle_digest` than the lock pins (draft-anywhere: the edit may live in the shared dir or any
/// native copy). A missing/unscannable source is reported as no-draft (nothing to compare), never an
/// error.
fn is_draft(ctx: &Ctx<'_>, map_path: &Path, lock: &Lock) -> Result<bool, ClientError> {
    let Some(map): Option<PlacementMap> = doc::read_map(ctx.fs, map_path)? else {
        return Ok(false);
    };
    for placement in &map.placements {
        let source = Path::new(placement);
        if !source.exists() {
            continue;
        }
        if let Ok(ScannedBundle { bundle_digest, .. }) = scan::scan(source)
            && to_hex(&bundle_digest) != lock.bundle_digest
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The SOURCE / STATUS / CAUSE columns for one tracked row, from the offline signals: the follow entry
/// (following flag + the per-device exclusion marker), the draft flag, an import origin host, the
/// workspace's friendly label, whether a workspace credential is held (signed-out detection), and the
/// last-delivery cache entry + the locally-applied version (offline `behind` + `removed-upstream`).
///
/// `behind` reads the last reconcile's served version (the cache) against the local applied version — an
/// auto follower whose reconcile already applied the update stays `current`; a confirm-each follower with
/// a pending offer, or any device that has not re-synced since the plane moved, reads `behind`.
fn derive_columns(
    follow: Option<&FollowEntry>,
    draft: bool,
    origin_host: Option<String>,
    ws_label: Option<&String>,
    has_credential: bool,
    delivered: Option<&DeliveredSkill>,
    local_version: &str,
) -> (Option<String>, Option<SkillStatus>, Option<DetachCause>) {
    // A purely local, never-followed, non-imported skill carries no columns (its absent workspace already
    // says "local") — leaving the pinned `list` shape byte-identical for that common case.
    if follow.is_none() && origin_host.is_none() {
        return (None, None, None);
    }
    // SOURCE: an imported skill names its origin host; a followed one its workspace label; else local.
    let source = origin_host
        .or_else(|| ws_label.cloned())
        .unwrap_or_else(|| "local".to_owned());

    // CAUSE (only when detached): an UPSTREAM withdrawal (the skill is still followed, so it outranks the
    // person/device causes), the per-device exclusion, a person-scoped unfollow, or signed-out — checked
    // most-specific first.
    let cause = if delivered.is_some_and(|d| d.withdrawn) {
        Some(DetachCause::RemovedUpstream)
    } else {
        match follow {
            Some(f) if f.excluded_here => Some(DetachCause::ExcludedHere),
            Some(f) if !f.following => Some(DetachCause::Unfollowed),
            Some(_) if !has_credential => Some(DetachCause::SignedOut),
            _ => None,
        }
    };
    let status = if cause.is_some() {
        SkillStatus::Detached
    } else if draft {
        SkillStatus::Draft
    } else if is_behind(delivered, local_version) {
        SkillStatus::Behind
    } else {
        SkillStatus::Current
    };
    (Some(source), Some(status), cause)
}

/// Whether the last reconcile served a version this copy has not applied — the offline `behind` signal.
/// Guards the never-received baseline (an all-zero local version is an unaccepted first receive, not
/// "behind") and an empty served version (a withdrawn / uncached skill).
fn is_behind(delivered: Option<&DeliveredSkill>, local_version: &str) -> bool {
    let Some(d) = delivered else { return false };
    !d.served_version.is_empty()
        && !local_version.bytes().all(|b| b == b'0')
        && d.served_version != local_version
}

/// Filter the tracked rows to the union of what the [`ListFilter`] names, ALL-OR-NONE: every positional
/// name and `--skill` selector must match at least one tracked skill (else [`ClientError::NoSuchSkill`]),
/// and every `--channel` selector must match at least one skill the last delivery cached as delivered via
/// that channel (else the uniform not-found). The rows are kept in their existing order.
fn apply_filter(
    tracked: Vec<(String, SkillEntry)>,
    filter: &ListFilter,
    sync: &sync_status::SyncStatus,
) -> Result<Vec<(String, SkillEntry)>, ClientError> {
    let mut keep: HashSet<String> = HashSet::new();
    // Names + `--skill` selectors: matched by skill name.
    for name in filter.names.iter().chain(filter.skills.iter()) {
        let matched: Vec<&String> = tracked
            .iter()
            .filter(|(_, e)| &e.skill == name)
            .map(|(id, _)| id)
            .collect();
        if matched.is_empty() {
            return Err(ClientError::NoSuchSkill { name: name.clone() });
        }
        keep.extend(matched.into_iter().cloned());
    }
    // `--channel` selectors: matched by the last delivery's cached `via` channels.
    for ch in &filter.channels {
        let mut matched = false;
        for (id, e) in &tracked {
            let via = e
                .workspace_id
                .as_deref()
                .and_then(|w| sync.workspaces.get(w))
                .and_then(|ws| ws.delivered.get(id))
                .map(|d| d.via_channels.as_slice())
                .unwrap_or(&[]);
            if via.iter().any(|c| c == ch) {
                keep.insert(id.clone());
                matched = true;
            }
        }
        if !matched {
            return Err(crate::resolve::not_found(ch));
        }
    }
    Ok(tracked
        .into_iter()
        .filter(|(id, _)| keep.contains(id))
        .collect())
}

/// The host of an import source (`github.com/owner/repo` → `github.com`), or `None` for an empty source.
fn origin_host(source: &str) -> Option<String> {
    source
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .filter(|h| !h.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use topos_harness::ClaudeCode;
    use topos_types::PERSISTED_SCHEMA_VERSION;
    use topos_types::persisted::Lock;
    use topos_types::requests::{WireSkillIndex, WireSkillIndexEntry};

    use super::*;
    use crate::ctx::Ctx;
    use crate::fs_seam::{FsOps, RealFs};
    use crate::ids::{RealClock, RealIds};
    use crate::plane::{InertFollow, InertPlane};
    use crate::sidecar::Layout;

    // 64-char lowercase-hex version ids (the schema-pinned shape).
    const VER_A: &str = "aa"; // repeated ×32 below
    const VER_B: &str = "bb";
    const VER_C: &str = "cc";
    const VER_X: &str = "dd";
    const DIGEST: &str = "ee";

    fn hex(byte: &str) -> String {
        byte.repeat(32)
    }

    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("topos-listrem-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake catalog transport: canned per-workspace responses (`Ok` index or a transport fault),
    /// capturing every `workspace_id` the caller reads (the real transport presents the workspace's Bearer
    /// credential; the merge logic under test only needs to know which workspaces were read).
    struct FakeCatalog {
        ok: HashMap<String, WireSkillIndex>,
        fail: HashSet<String>,
        calls: RefCell<Vec<String>>,
    }
    impl CatalogSource for FakeCatalog {
        fn fetch_catalog(&self, workspace_id: &str) -> Result<WireSkillIndex, PlaneError> {
            self.calls.borrow_mut().push(workspace_id.to_owned());
            if self.fail.contains(workspace_id) {
                return Err(PlaneError::Unavailable("boom".into()));
            }
            Ok(self
                .ok
                .get(workspace_id)
                .cloned()
                .unwrap_or(WireSkillIndex { skills: Vec::new() }))
        }
    }

    fn catalog_entry(skill_id: &str, version: &str) -> WireSkillIndexEntry {
        WireSkillIndexEntry {
            skill_id: skill_id.to_owned(),
            name: skill_id.to_owned(),
            kind: "skill".to_owned(),
            status: "active".to_owned(),
            version_id: hex(version),
            bundle_digest: hex(DIGEST),
            generation: 1,
            display_name: Some(skill_id.to_owned()),
            updated_at: 1,
            open_proposals: 0,
        }
    }

    /// Lay a tracked skill dir (`skills/<id>/lock.json` on `version`) so the tracked walk finds it and the
    /// local-version map records it.
    fn lay_skill(fs: &RealFs, layout: &Layout, id: &str, name: &str, version: &str) {
        let sid = crate::id::SkillId::parse(id).unwrap();
        fs.create_dir_all(&layout.skill_dir(&sid)).unwrap();
        doc::write_doc(
            fs,
            &layout.published(&sid).lock,
            &Lock {
                schema_version: PERSISTED_SCHEMA_VERSION,
                skill_id: id.to_owned(),
                name: name.to_owned(),
                base_commit: hex(version),
                bundle_digest: hex(DIGEST),
                files: Vec::new(),
            },
        )
        .unwrap();
    }

    fn follow_entry(skill_id: &str, workspace_id: &str, following: bool) -> FollowEntry {
        FollowEntry {
            skill_id: skill_id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            mode: FollowModeDoc::Auto,
            review_required: false,
            following,
            excluded_here: false,
            agents: Vec::new(),
            excluded_agents: Vec::new(),
        }
    }

    #[test]
    fn remote_merges_follow_state_and_degrades_a_failed_workspace() {
        let home = scratch("merge");
        let layout = Layout::new(&home);
        let fs = RealFs;

        // Local state: two followed skills in w_acme (one on VER_A, one on VER_B); s_docs not followed.
        lay_skill(&fs, &layout, "s_deploy", "deploy", VER_A);
        lay_skill(&fs, &layout, "s_runbook", "runbook", VER_B);
        enroll::write_follows_merged(
            &fs,
            &layout,
            &[
                follow_entry("s_deploy", "w_acme", true),
                follow_entry("s_runbook", "w_acme", true),
            ],
        )
        .unwrap();

        // The catalog for w_acme: s_deploy@A (== local → Following), s_docs@X (not followed → Available),
        // s_runbook@C (!= local B → FollowingBehind). w_beta's read fails (degrades to a warning).
        let mut ok = HashMap::new();
        ok.insert(
            "w_acme".to_owned(),
            WireSkillIndex {
                skills: vec![
                    catalog_entry("s_deploy", VER_A),
                    catalog_entry("s_docs", VER_X),
                    catalog_entry("s_runbook", VER_C),
                ],
            },
        );
        let fake = FakeCatalog {
            ok,
            fail: HashSet::from(["w_beta".to_owned()]),
            calls: RefCell::new(Vec::new()),
        };

        let ids = RealIds;
        let clock = RealClock;
        let plane = InertPlane;
        let follow = InertFollow;
        let harness = ClaudeCode::new(scratch("adapter"), &fs);
        let ctx = Ctx {
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: String::new(),
            layout: layout.clone(),
            harness: &harness,
            plane: &plane,
            follow: &follow,
            roots: None,
        };

        let scope = RemoteScope {
            catalog: &fake,
            memberships: vec![
                ("w_acme".to_owned(), "Acme".to_owned()),
                ("w_beta".to_owned(), "Beta".to_owned()),
            ],
            only: None,
        };
        let out = list(&ctx, None, false, None, Some(scope)).unwrap();

        // Partial results: w_acme's three skills land even though w_beta failed. Sorted by skill_id.
        let remote = &out.data.remote_available;
        assert_eq!(remote.len(), 3, "{remote:?}");
        assert_eq!(remote[0].skill_id, "s_deploy");
        assert_eq!(remote[0].state, RemoteFollowState::Following);
        assert_eq!(remote[1].skill_id, "s_docs");
        assert_eq!(remote[1].state, RemoteFollowState::Available);
        assert_eq!(remote[2].skill_id, "s_runbook");
        assert_eq!(remote[2].state, RemoteFollowState::FollowingBehind);
        assert!(remote.iter().all(|r| r.workspace_id == "w_acme"));

        // The failed workspace degraded to exactly one warning (never failing the whole list).
        assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
        assert!(out.warnings[0].contains("Beta"), "{:?}", out.warnings);

        // Both workspaces were read (the transport presents each one's Bearer credential internally).
        let calls = fake.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert!(calls.contains(&"w_acme".to_owned()) && calls.contains(&"w_beta".to_owned()));
    }

    #[test]
    fn remote_workspace_filter_narrows_to_one() {
        let home = scratch("filter");
        let layout = Layout::new(&home);
        let fs = RealFs;

        let mut ok = HashMap::new();
        ok.insert(
            "w_acme".to_owned(),
            WireSkillIndex {
                skills: vec![catalog_entry("s_docs", VER_X)],
            },
        );
        ok.insert(
            "w_beta".to_owned(),
            WireSkillIndex {
                skills: vec![catalog_entry("s_other", VER_A)],
            },
        );
        let fake = FakeCatalog {
            ok,
            fail: HashSet::new(),
            calls: RefCell::new(Vec::new()),
        };

        let ids = RealIds;
        let clock = RealClock;
        let plane = InertPlane;
        let follow = InertFollow;
        let harness = ClaudeCode::new(scratch("adapter2"), &fs);
        let ctx = Ctx {
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: String::new(),
            layout: layout.clone(),
            harness: &harness,
            plane: &plane,
            follow: &follow,
            roots: None,
        };

        // `--workspace w_beta` → only w_beta's catalog is read.
        let scope = RemoteScope {
            catalog: &fake,
            memberships: vec![
                ("w_acme".to_owned(), "Acme".to_owned()),
                ("w_beta".to_owned(), "Beta".to_owned()),
            ],
            only: Some("w_beta".to_owned()),
        };
        let out = list(&ctx, None, false, None, Some(scope)).unwrap();
        assert_eq!(out.data.remote_available.len(), 1);
        assert_eq!(out.data.remote_available[0].skill_id, "s_other");
        assert_eq!(out.data.remote_available[0].workspace_id, "w_beta");
        // Only the filtered workspace was contacted.
        assert_eq!(fake.calls.borrow().len(), 1);
        assert_eq!(fake.calls.borrow()[0], "w_beta");
    }

    #[test]
    fn remote_is_skipped_and_warns_when_narrowed_to_a_skill() {
        let home = scratch("narrowed");
        let layout = Layout::new(&home);
        let fs = RealFs;
        // A tracked skill so the name narrows cleanly (list <skill> requires exactly one match).
        lay_skill(&fs, &layout, "s_docs", "docs", VER_X);

        let mut ok = HashMap::new();
        ok.insert(
            "w_acme".to_owned(),
            WireSkillIndex {
                skills: vec![catalog_entry("s_docs", VER_X)],
            },
        );
        let fake = FakeCatalog {
            ok,
            fail: HashSet::new(),
            calls: RefCell::new(Vec::new()),
        };

        let ids = RealIds;
        let clock = RealClock;
        let plane = InertPlane;
        let follow = InertFollow;
        let harness = ClaudeCode::new(scratch("adapter3"), &fs);
        let ctx = Ctx {
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: String::new(),
            layout: layout.clone(),
            harness: &harness,
            plane: &plane,
            follow: &follow,
            roots: None,
        };
        let scope = RemoteScope {
            catalog: &fake,
            memberships: vec![("w_acme".to_owned(), "Acme".to_owned())],
            only: None,
        };
        // `list docs --remote`: the catalog is a bare-sweep browse, so a name-narrowed list SKIPS it with a
        // warning and attempts NO catalog read — the narrowing can never mislabel a followed skill.
        let out = list(&ctx, Some("docs"), false, None, Some(scope)).unwrap();
        assert!(out.data.remote_available.is_empty());
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("bare `topos list --remote`"))
        );
        assert!(
            fake.calls.borrow().is_empty(),
            "no catalog read is attempted when narrowed to a skill"
        );
    }

    /// A local ctx over `layout` — the offline column/filter derivations need no plane.
    fn local_ctx<'a>(
        fs: &'a RealFs,
        ids: &'a RealIds,
        clock: &'a RealClock,
        harness: &'a ClaudeCode,
        plane: &'a InertPlane,
        follow: &'a InertFollow,
        layout: &Layout,
    ) -> Ctx<'a> {
        Ctx {
            fs,
            ids,
            clock,
            device_id: String::new(),
            layout: layout.clone(),
            harness,
            plane,
            follow,
            roots: None,
        }
    }

    /// Seed one workspace's delivery cache in `sync_status.json` (served version + withdrawn + via).
    fn seed_delivered(fs: &RealFs, layout: &Layout, ws: &str, entries: &[(&str, DeliveredSkill)]) {
        crate::sync_status::record(
            fs,
            layout,
            &[(
                ws.to_owned(),
                crate::sync_status::WorkspaceSync {
                    last_delivery_at: Some(1),
                    staleness_window_ms: 604_800_000,
                    delivered: entries
                        .iter()
                        .map(|(id, d)| ((*id).to_owned(), d.clone()))
                        .collect(),
                    ..Default::default()
                },
            )],
        )
        .unwrap();
    }

    #[test]
    fn offline_behind_and_removed_upstream_come_from_the_delivery_cache() {
        let home = scratch("cache-cols");
        let layout = Layout::new(&home);
        let fs = RealFs;
        // Two followed skills (auto), a credential present (so neither is "signed out").
        lay_skill(&fs, &layout, "s_beh", "beh", VER_A); // local applied @A
        lay_skill(&fs, &layout, "s_gone", "gone", VER_B);
        enroll::write_follows_merged(
            &fs,
            &layout,
            &[
                follow_entry("s_beh", "w_acme", true),
                follow_entry("s_gone", "w_acme", true),
            ],
        )
        .unwrap();
        enroll::write_credentials(&fs, &layout, "wsc", "dev_1").unwrap();
        // The cache: `beh` was last served @C (≠ local A → behind); `gone` was withdrawn.
        seed_delivered(
            &fs,
            &layout,
            "w_acme",
            &[
                (
                    "s_beh",
                    DeliveredSkill {
                        served_version: hex(VER_C),
                        withdrawn: false,
                        via_channels: vec!["eng".into()],
                    },
                ),
                (
                    "s_gone",
                    DeliveredSkill {
                        withdrawn: true,
                        ..DeliveredSkill::default()
                    },
                ),
            ],
        );

        let (ids, clock, harness, plane, follow) = (
            RealIds,
            RealClock,
            ClaudeCode::new(scratch("adapter-cc"), &fs),
            InertPlane,
            InertFollow,
        );
        let ctx = local_ctx(&fs, &ids, &clock, &harness, &plane, &follow, &layout);
        let rows = list(&ctx, None, false, None, None).unwrap().data.tracked;

        let beh = rows.iter().find(|e| e.skill == "beh").unwrap();
        assert_eq!(beh.status, Some(SkillStatus::Behind));
        assert!(beh.cause.is_none());
        let gone = rows.iter().find(|e| e.skill == "gone").unwrap();
        assert_eq!(gone.status, Some(SkillStatus::Detached));
        assert_eq!(gone.cause, Some(DetachCause::RemovedUpstream));
    }

    #[test]
    fn channel_and_skill_selectors_filter_rows_all_or_none() {
        let home = scratch("filters");
        let layout = Layout::new(&home);
        let fs = RealFs;
        lay_skill(&fs, &layout, "s_deploy", "deploy", VER_A);
        lay_skill(&fs, &layout, "s_docs", "docs", VER_B);
        lay_skill(&fs, &layout, "s_lint", "lint", VER_C);
        enroll::write_follows_merged(
            &fs,
            &layout,
            &[
                follow_entry("s_deploy", "w_acme", true),
                follow_entry("s_docs", "w_acme", true),
                follow_entry("s_lint", "w_acme", true),
            ],
        )
        .unwrap();
        enroll::write_credentials(&fs, &layout, "wsc", "dev_1").unwrap();
        // `deploy` + `docs` ride channel `eng`; `lint` rides `release`.
        seed_delivered(
            &fs,
            &layout,
            "w_acme",
            &[
                ("s_deploy", via(&["eng"])),
                ("s_docs", via(&["eng"])),
                ("s_lint", via(&["release"])),
            ],
        );

        let (ids, clock, harness, plane, follow) = (
            RealIds,
            RealClock,
            ClaudeCode::new(scratch("adapter-f"), &fs),
            InertPlane,
            InertFollow,
        );
        let ctx = local_ctx(&fs, &ids, &clock, &harness, &plane, &follow, &layout);

        // `--channel eng` keeps deploy + docs, drops lint.
        let by_channel = list_with(
            &ctx,
            &ListFilter {
                channels: vec!["eng".into()],
                ..Default::default()
            },
            false,
            None,
            None,
            crate::ops::RowPage::unlimited(),
        )
        .unwrap()
        .data
        .tracked;
        let names: Vec<&str> = by_channel.iter().map(|e| e.skill.as_str()).collect();
        assert_eq!(names, vec!["deploy", "docs"]);

        // `--skill deploy --skill lint` keeps exactly those two (a name selector, union).
        let by_skill = list_with(
            &ctx,
            &ListFilter {
                skills: vec!["deploy".into(), "lint".into()],
                ..Default::default()
            },
            false,
            None,
            None,
            crate::ops::RowPage::unlimited(),
        )
        .unwrap()
        .data
        .tracked;
        let names: Vec<&str> = by_skill.iter().map(|e| e.skill.as_str()).collect();
        assert_eq!(names, vec!["deploy", "lint"]);

        // ALL-OR-NONE: an unknown `--skill` refuses the whole invocation.
        assert!(matches!(
            list_with(
                &ctx,
                &ListFilter {
                    skills: vec!["deploy".into(), "ghost".into()],
                    ..Default::default()
                },
                false,
                None,
                None,
                crate::ops::RowPage::unlimited(),
            ),
            Err(ClientError::NoSuchSkill { .. })
        ));
        // A `--channel` matching no delivered skill is the uniform not-found.
        assert!(matches!(
            list_with(
                &ctx,
                &ListFilter {
                    channels: vec!["nope".into()],
                    ..Default::default()
                },
                false,
                None,
                None,
                crate::ops::RowPage::unlimited(),
            ),
            Err(ClientError::TargetNotFound { .. })
        ));
    }

    fn via(channels: &[&str]) -> DeliveredSkill {
        DeliveredSkill {
            served_version: hex(VER_A),
            withdrawn: false,
            via_channels: channels.iter().map(|c| (*c).to_owned()).collect(),
        }
    }
}
