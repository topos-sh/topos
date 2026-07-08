//! `list [<skill>] [--footprint]` — inventory this machine. Populates the **tracked** bucket (every
//! skill with a local sidecar record) and, once enrolled, the **followed** bucket (the tracked subset
//! `follows.json` says is following its workspace `current`) plus a TTY enrollment header (workspace,
//! plane, currency-hook state) — the one-command answer to "am I enrolled, what am I following, is the
//! hook armed". `published_by_you` stays empty: the client keeps no durable record of its own settled
//! publishes (the op-WAL is deleted once an op settles; `lock.json` records no author), so that bucket
//! honestly waits for the plane-side `log --team` read. `untracked` needs harness discovery wiring and
//! renders empty. `--footprint` reports every topos-owned path outside skill dirs: the `~/.topos/` tree
//! plus any harness config the currency hook lives in (disclosed, never deleted).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use topos_core::digest::to_hex;
use topos_harness::registry::{self, SkillScope};
use topos_types::persisted::{Lock, PlacementMap};
use topos_types::requests::WireSkillIndexEntry;
use topos_types::results::{
    ListData, RemoteFollowState, RemoteSkillEntry, SkillEntry, UntrackedEntry,
};

use crate::ctx::Ctx;
use crate::device_signer::DeviceSigner;
use crate::enroll::{self, FollowEntry, FollowModeDoc};
use crate::error::ClientError;
use crate::plane::{CatalogSource, PlaneError};
use crate::scan;
use crate::sidecar;
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
/// entry with local follow-state. `pub(crate)` — built by the composition root (real `ureq` transport +
/// device signer + the memberships from `user.json`) and by the test (a fake transport). Present only
/// under `--remote`; `None` is the local-only path.
pub(crate) struct RemoteScope<'a> {
    /// The device-signed catalog transport (`GET /v1/workspaces/{ws}/skills`).
    pub catalog: &'a dyn CatalogSource,
    /// The device signer — signs each per-workspace catalog read and carries the `device_key_id` selector.
    pub signer: &'a DeviceSigner,
    /// Every workspace this install has joined, as `(workspace_id, display_label)` (from `user.json`) — the
    /// catalog targets.
    pub memberships: Vec<(String, String)>,
    /// The global `--workspace <id>` filter (narrows to one joined workspace); `None` = every joined one.
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
    /// The pinned plane's base URL.
    pub base_url: String,
    /// Whether the harness session-start currency hook is currently installed (read from the adapter's
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

/// Inventory the tracked skills, optionally narrowed to one name and/or with the footprint, and — under
/// `--remote` ([`RemoteScope`] present) — the followed workspaces' catalogs annotated with local
/// follow-state (a per-workspace transport fault DEGRADES to a warning, never failing the whole `list`).
///
/// # Errors
/// [`ClientError::NoSuchSkill`] / [`ClientError::AmbiguousName`] when a name filter does not resolve to
/// exactly one skill; otherwise a read failure.
pub(crate) fn list(
    ctx: &Ctx<'_>,
    skill: Option<&str>,
    want_footprint: bool,
    discover: Option<DiscoveryRoots>,
    remote: Option<RemoteScope<'_>>,
) -> Result<ListOutcome, ClientError> {
    // The follow-state is the ONE source for the per-skill workspace provenance, the followed bucket, and
    // the TTY notes — read it once here (absent ⇒ empty, e.g. unenrolled or a membership-only door). We
    // deliberately do NOT consult `ctx.follow`: `list` already keys its followed bucket + notes off this
    // file read, so the per-entry `workspace_id` shares that single authority (they can only agree).
    let follows = enroll::read_follows(ctx.fs, &ctx.layout)?
        .map(|f| f.follows)
        .unwrap_or_default();

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
        // The skill's workspace provenance, from its follow entry (a retained-but-paused entry still
        // carries it); `None` for a purely local, never-followed `add`'d skill.
        let workspace_id = follows
            .iter()
            .find(|f| f.skill_id == id_str)
            .map(|f| f.workspace_id.clone());
        tracked.push((
            id_str,
            SkillEntry {
                skill: lock.name,
                workspace_id,
                version_id: lock.base_commit,
                bundle_digest: lock.bundle_digest,
                draft,
                pending_proposals: Vec::new(),
            },
        ));
    }
    // Deterministic order (name, then version).
    tracked.sort_by(|a, b| {
        a.1.skill
            .cmp(&b.1.skill)
            .then_with(|| a.1.version_id.cmp(&b.1.version_id))
    });

    if let Some(want) = skill {
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
    }

    // The enrolled-state disclosure + the followed bucket, from the same docs the pull engine reads.
    // `instance.json` present = enrolled (its presence is what `follow` writes); `follows.json` may be
    // absent (a membership-only enrollment). A followed skill always has a sidecar record (`follow` lays
    // the first-receive baseline), so the followed bucket is the tracked subset its ids select; a
    // follows entry with no local record (a foreign/partial state) is simply not listable yet.
    let enrollment = match enroll::read_instance(ctx.fs, &ctx.layout)? {
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
                            let label = m.display_name.unwrap_or_else(|| m.workspace_id.clone());
                            (m.workspace_id, label)
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(ListEnrollment {
                workspace_labels,
                base_url: instance.base_url,
                hook_active: !ctx.harness.uninstall_footprint().is_empty(),
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

    // Discover untracked skills across the baked harness registry — only on a bare sweep (a name-narrowed
    // `list` is about that one tracked skill) and only when not `--tracked`. Dedups against every tracked
    // placement so an adopted/followed skill never shows up as "untracked".
    let untracked = match (&discover, skill) {
        (Some(roots), None) => discover_untracked(ctx, roots)?,
        _ => Vec::new(),
    };

    // The `--remote` catalog: for each followed workspace, a device-signed catalog read merged with the
    // local follow-state. A per-workspace transport fault degrades to a warning (never fails the `list`).
    let mut warnings: Vec<String> = Vec::new();
    let remote_available = match remote {
        Some(scope) => build_remote(&scope, &follows, &local_versions, &mut warnings),
        None => Vec::new(),
    };

    Ok(ListOutcome {
        data: ListData {
            followed,
            published_by_you: Vec::new(),
            tracked,
            untracked,
            remote_available,
            footprint,
        },
        enrollment,
        warnings,
    })
}

/// Read each target workspace's catalog (device-signed) and merge every entry with the local follow-state.
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
        // Sign the catalog read for THIS workspace (the signature binds the workspace id + device key id).
        let signature = match scope.signer.sign_catalog_read(ws_id) {
            Ok(sig) => sig,
            Err(_) => {
                warnings.push(format!(
                    "could not sign the catalog read for workspace {ws_label} — skipped"
                ));
                continue;
            }
        };
        let index =
            match scope
                .catalog
                .fetch_catalog(ws_id, scope.signer.device_key_id(), &signature)
            {
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
/// silently skipped, never an error.
fn discover_untracked(
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
            doc::read_doc(ctx.fs, &ctx.layout.published(&id).map)?
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

/// A skill carries a draft iff the live source bytes hash to a different `bundle_digest` than the lock
/// pins. A missing/unscannable source is reported as no-draft (nothing to compare), never an error.
fn is_draft(ctx: &Ctx<'_>, map_path: &Path, lock: &Lock) -> Result<bool, ClientError> {
    let Some(map): Option<PlacementMap> = doc::read_doc(ctx.fs, map_path)? else {
        return Ok(false);
    };
    let Some(placement) = map.placements.first() else {
        return Ok(false);
    };
    let source = Path::new(placement);
    if !source.exists() {
        return Ok(false);
    }
    match scan::scan(source) {
        Ok(ScannedBundle { bundle_digest, .. }) => Ok(to_hex(&bundle_digest) != lock.bundle_digest),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use topos_core::sign::{CatalogReadFields, verify_catalog_read};
    use topos_harness::ClaudeCode;
    use topos_types::persisted::Lock;
    use topos_types::requests::{WireSkillIndex, WireSkillIndexEntry};
    use topos_types::{Generation, PERSISTED_SCHEMA_VERSION};

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

    /// A fake device-signed catalog transport: canned per-workspace responses (`Ok` index or a transport
    /// fault), capturing every `(workspace_id, device_key_id, signature)` so the test can prove the caller
    /// signed the correct per-workspace frame.
    struct FakeCatalog {
        ok: HashMap<String, WireSkillIndex>,
        fail: HashSet<String>,
        calls: RefCell<Vec<(String, String, [u8; 64])>>,
    }
    impl CatalogSource for FakeCatalog {
        fn fetch_catalog(
            &self,
            workspace_id: &str,
            device_key_id: &str,
            signature: &[u8; 64],
        ) -> Result<WireSkillIndex, PlaneError> {
            self.calls.borrow_mut().push((
                workspace_id.to_owned(),
                device_key_id.to_owned(),
                *signature,
            ));
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
            version_id: hex(version),
            bundle_digest: hex(DIGEST),
            generation: Generation { epoch: 1, seq: 1 },
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
            read_token: "rt_secret".to_owned(),
            mode: FollowModeDoc::Auto,
            review_required: false,
            following,
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
        let signer = DeviceSigner::load_or_generate(&fs, &layout).unwrap();

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
            plane_key: [0u8; 32],
            follow: &follow,
        };

        let scope = RemoteScope {
            catalog: &fake,
            signer: &signer,
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

        // Both workspaces were signed + read; each captured signature verifies over the per-workspace
        // frame the plane rebuilds (the caller signed the correct workspace, with this device's key id).
        let calls = fake.calls.borrow();
        assert_eq!(calls.len(), 2);
        for (ws, key_id, sig) in calls.iter() {
            assert_eq!(key_id, signer.device_key_id());
            let fields = CatalogReadFields {
                workspace_id: ws,
                device_key_id: signer.device_key_id(),
            };
            assert!(
                verify_catalog_read(&fields, sig, &signer.public_key()),
                "signature must verify for workspace {ws}"
            );
        }
    }

    #[test]
    fn remote_workspace_filter_narrows_to_one() {
        let home = scratch("filter");
        let layout = Layout::new(&home);
        let fs = RealFs;
        let signer = DeviceSigner::load_or_generate(&fs, &layout).unwrap();

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
            plane_key: [0u8; 32],
            follow: &follow,
        };

        // `--workspace w_beta` → only w_beta's catalog is read.
        let scope = RemoteScope {
            catalog: &fake,
            signer: &signer,
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
        assert_eq!(fake.calls.borrow()[0].0, "w_beta");
    }
}
