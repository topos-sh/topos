//! `list [<skill>] [--footprint]` — inventory this machine. Populates the **tracked** bucket (every
//! skill with a local sidecar record) and, once enrolled, the **followed** bucket (the tracked subset
//! `follows.json` says is following its workspace `current`) plus a TTY enrollment header (workspace,
//! plane, currency-hook state) — the one-command answer to "am I enrolled, what am I following, is the
//! hook armed". `published_by_you` stays empty: the client keeps no durable record of its own settled
//! publishes (the op-WAL is deleted once an op settles; `lock.json` records no author), so that bucket
//! honestly waits for the plane-side `log --team` read. `untracked` needs harness discovery wiring and
//! renders empty. `--footprint` reports every topos-owned path outside skill dirs: the `~/.topos/` tree
//! plus any harness config the currency hook lives in (disclosed, never deleted).

use std::path::Path;

use topos_core::digest::to_hex;
use topos_types::persisted::{Lock, PlacementMap};
use topos_types::results::{ListData, SkillEntry};

use crate::ctx::Ctx;
use crate::enroll::{self, FollowModeDoc};
use crate::error::ClientError;
use crate::scan;
use crate::sidecar;
use crate::{doc, scan::ScannedBundle};

/// A `list` run's typed result: the schema-pinned envelope payload plus the TTY-only enrollment
/// disclosure. `ListData` is PINNED (its buckets carry `SkillEntry` rows only), so the enrollment header
/// and the per-row follow annotations ride alongside for the TTY renderer — mirroring how `pull`'s
/// warnings ride outside `PullData`.
#[derive(Debug)]
pub(crate) struct ListOutcome {
    pub data: ListData,
    /// `Some` iff enrolled (`instance.json` present — the same presence rule `load_enrollment` uses).
    pub enrollment: Option<ListEnrollment>,
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
    /// `false` = the entry is retained but unfollowed (`topos follow --approve <skill>` resumes it).
    pub following: bool,
}

/// Inventory the tracked skills, optionally narrowed to one name and/or with the footprint.
///
/// # Errors
/// [`ClientError::NoSuchSkill`] / [`ClientError::AmbiguousName`] when a name filter does not resolve to
/// exactly one skill; otherwise a read failure.
pub(crate) fn list(
    ctx: &Ctx<'_>,
    skill: Option<&str>,
    want_footprint: bool,
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

    Ok(ListOutcome {
        data: ListData {
            followed,
            published_by_you: Vec::new(),
            tracked,
            untracked: Vec::new(),
            footprint,
        },
        enrollment,
    })
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
