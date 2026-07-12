//! The skill lifecycle — archive / unarchive / delete / purge (the orchestration half; the SQL +
//! the custody un-rooting live in `db/directory/catalog.rs`).
//!
//! These are OWNER acts of the web-surface class (PRODUCT's exhaustive web list), exposed as
//! PRIVILEGED lib-level session ops a hosted composition's authenticated pages call — the same
//! posture as the session roster/review legs: uniformly denied on self-host (the OSS web app picks
//! them up at the door cutover; the guarded SQL functions are the contract it will call), every
//! pre-gate miss the single indistinguishable [`AuthorityError::NotFound`], and the OWNER gate
//! answered inside the guarded function (a confirmed non-owner member gets the typed refusal — an
//! authenticated member is entitled to the real reason). Naturally idempotent state machines — no
//! op-id receipt ceremony (re-archiving an archived skill answers `NotActive`, not a duplicate);
//! the step-up/type-the-name confirm is the calling page's ceremony, not this layer's.

use crate::Authority;
use crate::db::custody::witness::AccessWitness;
use crate::enroll::DeploymentMode;
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, Principal, WorkspaceId};

/// An archive / unarchive / delete outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleOutcome {
    /// Archived — renamed `<name>-archived-<date>` (a counter on same-day repeats), the base name
    /// freed, every channel placement removed, open proposals closed with author notices.
    Archived { archived_name: String },
    /// Unarchived — renamed back to the base name.
    Unarchived { name: String },
    /// Unarchive refused: the base name was reused by a new identity (keep the suffix, or rename).
    NameTaken,
    /// Archive refused: not active (already archived, or deleted).
    NotActive,
    /// Delete/unarchive refused: the skill is not archived (delete is archive-first).
    NotArchived,
    /// Deleted — the catalog row is the tombstone (under its archived name); content un-rooted for
    /// the GC. Deletion cannot recall device copies.
    Deleted,
    /// The acting member is not an owner (the typed role refusal).
    OwnerRoleRequired,
}

/// A version-purge outcome (the leak tool).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PurgeOutcome {
    /// The version's bytes are un-rooted (the next GC pass reclaims what no live version shares);
    /// its hash stays in history as a tombstone (who, when); dependent proposals closed with
    /// author notices.
    Purged,
    /// Refused: the version is still `current` — publish or revert first.
    IsCurrent,
    /// Already purged (idempotent information).
    AlreadyPurged,
    /// The acting member is not an owner.
    OwnerRoleRequired,
}

/// The shared session front door: self-host denied, canonical principal, CONFIRMED membership —
/// every miss the uniform `NotFound`. (The OWNER gate stays inside the guarded function.)
async fn session_member(
    authority: &Authority,
    ws: &WorkspaceId,
    acting_email: &str,
    plane_mode: DeploymentMode,
) -> Result<Principal> {
    if plane_mode == DeploymentMode::SelfHost {
        return Err(AuthorityError::NotFound);
    }
    let acting = Principal::parse(acting_email).map_err(|_| AuthorityError::NotFound)?;
    if !authority.db().read_gate(ws, &acting).await? {
        return Err(AuthorityError::NotFound);
    }
    Ok(acting)
}

/// The `YYYY-MM-DD` label the archive rename carries, taken from the RFC-3339 `created_at` the
/// caller already stamped (one clock, no second formatter).
fn date_label(created_at: &str) -> Result<&str> {
    created_at
        .get(..10)
        .filter(|d| d.len() == 10 && d.as_bytes()[4] == b'-' && d.as_bytes()[7] == b'-')
        .ok_or_else(|| AuthorityError::internal(BadCreatedAt))
}

pub(crate) async fn archive_skill_session(
    authority: &Authority,
    ws: &WorkspaceId,
    acting_email: &str,
    skill_name: &str,
    plane_mode: DeploymentMode,
    created_at: &str,
    now: i64,
) -> Result<LifecycleOutcome> {
    let acting = session_member(authority, ws, acting_email, plane_mode).await?;
    let date = date_label(created_at)?;
    authority
        .db()
        .archive_skill_txn(ws, skill_name, &acting, date, now, created_at)
        .await
}

pub(crate) async fn unarchive_skill_session(
    authority: &Authority,
    ws: &WorkspaceId,
    acting_email: &str,
    skill_name: &str,
    plane_mode: DeploymentMode,
) -> Result<LifecycleOutcome> {
    let acting = session_member(authority, ws, acting_email, plane_mode).await?;
    authority
        .db()
        .unarchive_skill_txn(ws, skill_name, &acting)
        .await
}

pub(crate) async fn delete_skill_session(
    authority: &Authority,
    ws: &WorkspaceId,
    acting_email: &str,
    skill_name: &str,
    plane_mode: DeploymentMode,
    now: i64,
) -> Result<LifecycleOutcome> {
    let acting = session_member(authority, ws, acting_email, plane_mode).await?;
    authority
        .db()
        .delete_skill_txn(ws, skill_name, &acting, now)
        .await
}

pub(crate) async fn purge_version_session(
    authority: &Authority,
    ws: &WorkspaceId,
    acting_email: &str,
    skill_name: &str,
    version: CommitId,
    plane_mode: DeploymentMode,
    created_at: &str,
    now: i64,
) -> Result<PurgeOutcome> {
    let acting = session_member(authority, ws, acting_email, plane_mode).await?;
    authority
        .db()
        .purge_version_txn(ws, skill_name, version, &acting, now, created_at)
        .await
}

#[derive(Debug, thiserror::Error)]
#[error("created_at is not an RFC-3339 timestamp (no date to label the archive rename with)")]
struct BadCreatedAt;
