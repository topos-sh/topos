//! Describe — the member-scoped READ ops the two-phase verbs render their "before" from, plus the two
//! member-lane WRITES that ride the same front door (the notices ack + the roster invite). The
//! orchestration half; the raw SQL + the guarded `topos_*` calls live in `db/directory/describe.rs`.
//!
//! Every op is authenticated by the ONE workspace credential and front-doored by the ONE membership
//! predicate — the SAME [`device_member`](crate::channels::device_member) gate the channel ops run —
//! so every pre-gate miss (unknown/revoked credential, non-member, unknown workspace/skill) is the
//! single indistinguishable [`AuthorityError::NotFound`]. The reads mint nothing durable; `ack_notices`
//! (the read-state write) and `invite` (a roster write) route through their guarded SQL functions like
//! every other policy write, so the database answer is authoritative for the web tier too. Commit
//! messages/authors are read from the git store **best-effort** — display-only facts (the
//! consent-critical fact is the re-verified `bundle_digest`), so an unreadable commit degrades to an
//! empty value, never an error.

use std::path::PathBuf;

use topos_gitstore::Store;

use crate::Authority;
use crate::authority::run_blocking;
use crate::channels::device_member;
use crate::db::directory::describe::{CatalogRow, SkillCommitRow};
use crate::error::{AuthorityError, Result};
use crate::id::{BundleId, Principal, WorkspaceId};

/// The caller's own membership facts (the `follow`/`invite` describe header, and a `me` read): the
/// workspace identity + its share address, the caller's seat, and who may invite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Me {
    pub workspace_id: String,
    /// The workspace's URL slug — `<link_base>/<name>` IS the share address.
    pub name: String,
    pub display_name: String,
    /// The workspace's share address (`<link_base>/<name>`) — the door a new member follows.
    pub address: String,
    pub principal: String,
    pub role: String,
    /// Who invited the caller (`None` for a genesis / self-standup seat).
    pub invited_by: Option<String>,
    /// `"members"` / `"owners"` — who may invite.
    pub invite_policy: String,
}

/// One OPEN proposal across the whole workspace — the review inbox's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalIndexEntry {
    pub skill_id: String,
    pub skill_name: String,
    pub version_id: [u8; 32],
    pub base_version_id: [u8; 32],
    pub proposer: String,
    /// The proposed version's git commit message (best-effort; `""` when the commit is unreadable).
    pub message: String,
    pub created_at: String,
    /// The base no longer equals `current` — the proposal must rebase before it can be approved.
    pub stale: bool,
}

/// A skill's full history for `log`: the catalog identity + status, its versions (newest-first where
/// walkable, then the unordered provenance tail), and its proposals (any status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillLog {
    pub skill_id: String,
    pub name: String,
    /// The catalog's bundle kind (`"skill"` today) — display metadata, no reader branches on it.
    pub kind: String,
    pub status: String,
    /// The pre-archive name — present only for an archived skill.
    pub base_name: Option<String>,
    pub versions: Vec<LogVersion>,
    pub proposals: Vec<LogProposal>,
}

/// One version in a [`SkillLog`]: the id, the (best-effort) display author + message, whether it is
/// the current pointer, and the version-purge tombstone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogVersion {
    pub version_id: [u8; 32],
    /// The commit's display author (`None` when the commit is unreadable — reclaimed history).
    pub author: Option<String>,
    /// The commit's message (`None` when unreadable).
    pub message: Option<String>,
    /// Whether this version is the one `current` points at.
    pub current: bool,
    /// When this version's bytes were purged (`None` while live) — the who/when tombstone.
    pub purged_at: Option<i64>,
    pub purged_by: Option<String>,
}

/// One proposal in a [`SkillLog`] (any status — the resolution facts included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogProposal {
    pub version_id: [u8; 32],
    pub proposer: String,
    pub status: String,
    pub resolved_by: Option<String>,
    pub resolved_reason: Option<String>,
    pub resolved_at: Option<String>,
    pub created_at: String,
}

/// A skill's audience (`reach`): how far a publish would actually travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reach {
    /// Confirmed members currently entitled to the skill (via any channel or a direct follow).
    pub persons: u64,
    /// Their non-revoked devices — how many endpoints the skill reaches.
    pub devices: u64,
}

/// The `invite` write's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteOutcome {
    /// The emails were seated as invited members (returned in their canonical, folded form).
    Invited { invited: Vec<String> },
    /// The workspace restricts inviting to owners and the caller is not one.
    OwnerRoleRequired,
    /// A named channel does not exist — nothing was written (resolve-all-or-apply-none).
    UnknownChannel,
}

/// `me` — the caller's membership facts + the workspace address.
pub(crate) async fn membership_describe(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
) -> Result<Me> {
    let identity = device_member(authority, ws, credential).await?;
    let row = authority
        .db()
        .membership_row(ws, &identity.principal)
        .await?
        .ok_or(AuthorityError::NotFound)?;
    // The share address rides the SAME link base the minted enrollment links do (one authoritative
    // copy on the enrollment config).
    let address = format!(
        "{}/{}",
        authority.enrollment()?.config.link_base(),
        row.name
    );
    Ok(Me {
        workspace_id: ws.as_str().to_owned(),
        name: row.name,
        display_name: row.display_name,
        address,
        principal: identity.principal.as_str().to_owned(),
        role: row.role,
        invited_by: row.invited_by,
        invite_policy: row.invite_policy,
    })
}

/// The review inbox: every OPEN proposal in the workspace, with the proposed version's commit message
/// (best-effort) and a derived `stale` flag.
pub(crate) async fn proposals_index(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
) -> Result<Vec<ProposalIndexEntry>> {
    device_member(authority, ws, credential).await?;
    let rows = authority.db().open_proposals_index(ws).await?;
    // Read each proposed version's commit message best-effort, opening the store ONCE on the blocking
    // pool (a per-proposal store open would be wasteful; an unreadable commit ⇒ an empty message).
    let commits: Vec<[u8; 32]> = rows.iter().map(|r| r.version_id).collect();
    let messages = read_messages(authority.workspace_git_dir(ws), commits).await?;
    Ok(rows
        .into_iter()
        .zip(messages)
        .map(|(r, message)| ProposalIndexEntry {
            skill_id: r.skill_id,
            skill_name: r.skill_name,
            version_id: r.version_id,
            base_version_id: r.base_version_id,
            proposer: r.proposer,
            message,
            created_at: r.created_at,
            stale: r.stale,
        })
        .collect())
}

/// `log <skill>` — a skill's version + proposal history. Resolves the NAME first, then the freed
/// base name of an archived successor (the hint that carries `log <old-name>` to the identity that
/// vacated it), then a bare skill id; anything else is the uniform miss.
pub(crate) async fn skill_log(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
    skill: &str,
) -> Result<SkillLog> {
    device_member(authority, ws, credential).await?;
    let catalog = match authority
        .db()
        .catalog_by_name_or_archived_base(ws, skill)
        .await?
    {
        Some(row) => row,
        None if BundleId::parse(skill).is_ok() => authority
            .db()
            .catalog_by_id(ws, skill)
            .await?
            .ok_or(AuthorityError::NotFound)?,
        None => return Err(AuthorityError::NotFound),
    };
    let CatalogRow {
        skill_id,
        name,
        kind,
        status,
        base_name,
    } = catalog;
    let sid = BundleId::parse(&skill_id).map_err(AuthorityError::integrity)?;
    let current = authority.db().read_current_commit(ws, &sid).await?;
    let current_commit = current.map(|c| c.0);
    // Every provenance row: the tombstone facts + the full version set for the unordered tail.
    let sc_rows = authority.db().skill_commit_log(ws, &sid).await?;
    // Walk `current`'s first-parent chain for ORDERED, display-rich history; the strict one-commit
    // meta read fails closed on an unmapped (purged/reclaimed) parent, which STOPS the walk cleanly —
    // the provenance tail below still lists the rest (author/message unknown).
    let walk = walk_first_parents(authority.workspace_git_dir(ws), current_commit).await?;

    let mut versions = Vec::with_capacity(sc_rows.len().max(walk.len()));
    let mut seen: std::collections::HashSet<[u8; 32]> = std::collections::HashSet::new();
    let tombstone = |vid: [u8; 32]| -> (Option<i64>, Option<String>) {
        sc_rows
            .iter()
            .find(|r| r.version_id == vid)
            .map_or((None, None), |r| (r.purged_at, r.purged_by.clone()))
    };
    for (version_id, author, message) in walk {
        seen.insert(version_id);
        let (purged_at, purged_by) = tombstone(version_id);
        versions.push(LogVersion {
            current: current_commit == Some(version_id),
            version_id,
            author: Some(author),
            message: Some(message),
            purged_at,
            purged_by,
        });
    }
    // The unordered tail: provenance rows the walk never reached (purged ancestors, unaccepted
    // candidates, or — for a currentless skill — its whole history).
    for SkillCommitRow {
        version_id,
        purged_at,
        purged_by,
    } in &sc_rows
    {
        if seen.insert(*version_id) {
            versions.push(LogVersion {
                current: current_commit == Some(*version_id),
                version_id: *version_id,
                author: None,
                message: None,
                purged_at: *purged_at,
                purged_by: purged_by.clone(),
            });
        }
    }
    let proposals = authority.db().skill_proposals_log(ws, &sid).await?;
    Ok(SkillLog {
        skill_id,
        name,
        kind,
        status,
        base_name,
        versions,
        proposals,
    })
}

/// `reach <skill>` — the skill's audience (confirmed members entitled to it + their non-revoked
/// devices). The name resolves at any catalog status (an archived name's entitlement is empty
/// anyway); an unknown name is the uniform miss.
pub(crate) async fn reach(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
    skill_id: &str,
) -> Result<Reach> {
    device_member(authority, ws, credential).await?;
    authority.db().reach(ws, skill_id).await
}

/// Ack a batch of the caller's own notices by id (the read-state write; idempotent — only the
/// person's own unacked rows move). Routes through the guarded `topos_notices_ack`.
pub(crate) async fn ack_notices(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
    ids: &[String],
    now: i64,
) -> Result<()> {
    let identity = device_member(authority, ws, credential).await?;
    authority
        .db()
        .ack_notices_txn(ws, &identity.principal, ids, now)
        .await
}

/// `invite` — seat one or more emails as invited members (optionally pre-placing them into channels),
/// through the guarded `topos_invite`. Member-level unless the workspace restricts inviting to owners.
pub(crate) async fn invite(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
    emails: &[String],
    channels: &[String],
    created_at: &str,
) -> Result<InviteOutcome> {
    let identity = device_member(authority, ws, credential).await?;
    // Fold each email through the canonical principal parse the governance ops use — an invalid
    // email is a typed argument error (`InvalidId`), never a silent drop or the uniform miss.
    let mut folded = Vec::with_capacity(emails.len());
    for email in emails {
        folded.push(Principal::parse(email)?);
    }
    authority
        .db()
        .invite_txn(ws, &identity.principal, &folded, channels, created_at)
        .await
}

/// Read a batch of commit messages best-effort, opening the workspace store ONCE on the blocking pool.
/// An unreadable commit (reclaimed history, an unopenable store) yields an empty message — display
/// only, never consent-critical, so never an error.
async fn read_messages(git_dir: PathBuf, commits: Vec<[u8; 32]>) -> Result<Vec<String>> {
    run_blocking(move || {
        let store = Store::open(&git_dir).ok();
        Ok(commits
            .into_iter()
            .map(|c| {
                store
                    .as_ref()
                    .and_then(|s| s.read_commit_meta(c).ok())
                    .map(|n| n.message)
                    .unwrap_or_default()
            })
            .collect())
    })
    .await
}

/// Walk `from`'s first-parent chain best-effort, returning `(version_id, author, message)` newest
/// first. The strict one-commit meta read fails closed on an unmapped parent (a purged/reclaimed
/// ancestor), which stops the walk — the caller lists the remainder from provenance.
async fn walk_first_parents(
    git_dir: PathBuf,
    from: Option<[u8; 32]>,
) -> Result<Vec<([u8; 32], String, String)>> {
    run_blocking(move || {
        let mut out = Vec::new();
        let Some(mut next) = from else {
            return Ok(out);
        };
        let Ok(store) = Store::open(&git_dir) else {
            return Ok(out);
        };
        while let Ok(node) = store.read_commit_meta(next) {
            let parent = node.parents.first().copied();
            out.push((node.version_id, node.author, node.message));
            match parent {
                Some(p) => next = p,
                None => break,
            }
        }
        Ok(out)
    })
    .await
}
