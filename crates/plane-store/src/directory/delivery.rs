//! Delivery — "what should this device have", and the fleet's applied-state report (the
//! orchestration half; the SQL lives in `db/directory/delivery.rs`).
//!
//! The delivery read is the currency hot path: the session-start hook calls it once per workspace,
//! and the client reconciles against the answer — install new, update moved, withdraw what upstream
//! no longer delivers, freeze what the person detached. The computation is the ONE entitlement
//! predicate (`topos_entitled_skills`, extending the confirmed-membership predicate every lane
//! gates on): DISTINCT union of roster-derived `everyone` ∪ followed channels ∪ direct follows −
//! unfollowed skills − this device's exclusions, active catalog entries only, skipping current-less
//! skills. Authentication is the device read lane's (credential sha256 → non-revoked row →
//! confirmed member; every miss the uniform `NotFound` — a member REMOVED from the roster reads
//! `NotFound` for the whole workspace, which the client treats as freeze-everything, never a clean).

use topos_types::Generation;

use crate::Authority;
use crate::db::custody::witness::AccessWitness;
use crate::db::directory::delivery::{EntitledDbRow, NoticeDbRow};
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, SkillId, WorkspaceId};

/// One skill this device should have — catalog identity, the pinned current version, the resolved
/// protection posture, and the `via` attribution the narration uses.
#[derive(Debug, Clone)]
pub struct DeliveredSkill {
    pub skill_id: String,
    /// The catalog's user-facing name (the on-disk directory name for a fresh install).
    pub name: String,
    pub display_name: Option<String>,
    /// `"open"` / `"reviewed"` — the resolved per-bundle cascade (the client's publish preflight
    /// posture; the server re-decides authoritatively on every write).
    pub protection: String,
    pub version_id: [u8; 32],
    pub generation: Generation,
    /// Epoch milliseconds of the last pointer move.
    pub updated_at: i64,
    pub bundle_digest: [u8; 32],
    /// The channels delivering it (names, sorted; `everyone` included when it does).
    pub via_channels: Vec<String>,
    /// Whether the person also follows it directly (survives every channel drop).
    pub direct: bool,
}

/// One unacked person-scoped notice (verdicts with reasons, circumstantial proposal closures — the
/// open `kind` vocabulary grows without a schema change). The silent hook fetches these without
/// acking; interactive narration acks by id (a later surface's write).
#[derive(Debug, Clone)]
pub struct DeliveryNotice {
    pub id: String,
    pub kind: String,
    pub skill_id: Option<String>,
    /// The skill's current catalog name (joined for narration; `None` when the notice names none).
    pub skill_name: Option<String>,
    pub version_id: Option<[u8; 32]>,
    pub actor: Option<String>,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    pub message: Option<String>,
    pub created_at: String,
}

/// The delivery response: the entitled set + the person's detached skills (freeze-in-place, never
/// clean — the who-acts principle needs the client to distinguish "you detached this" from
/// "upstream withdrew this", and absence alone cannot say which) + the notices feed + the
/// open-proposal count across the entitled set.
#[derive(Debug, Clone)]
pub struct Delivery {
    pub skills: Vec<DeliveredSkill>,
    /// Skill ids the person detached (unfollowed, or lapsed via a channel leave / removal) and that
    /// are NOT currently re-entitled — every device freezes these in place.
    pub detached: Vec<String>,
    /// Skill ids THIS DEVICE excludes ("not on this device") — the third actor: the copy leaves this
    /// device, the person keeps receiving it everywhere else, and `follow` here lifts it.
    pub excluded: Vec<String>,
    pub notices: Vec<DeliveryNotice>,
    /// OPEN, non-stale proposals across the entitled skills (the review-inbox pressure gauge; the
    /// inbox detail is a separate surface).
    pub proposals_awaiting: u64,
    /// The workspace's staleness window in milliseconds — the ONE clock the fleet page and the
    /// client's currency hook both read (`topos_staleness_window`; a missing policy row COALESCEs to
    /// the default), so a device knows how long its last apply stays "current".
    pub staleness_window_ms: u64,
}

/// One applied-state report row: what this device holds for a skill after its reconcile.
#[derive(Debug, Clone)]
pub struct AppliedSkill {
    pub skill_id: SkillId,
    pub version_id: CommitId,
}

pub(crate) async fn delivery(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
) -> Result<Delivery> {
    let credential_sha256 = topos_core::digest::sha256(credential.as_bytes());
    let identity = authority
        .db()
        .resolve_read_credential(ws, &credential_sha256)
        .await?
        .ok_or(AuthorityError::NotFound)?;
    if !authority.db().read_gate(ws, &identity.principal).await? {
        return Err(AuthorityError::NotFound);
    }
    // ONE snapshot for the three semantic reads: a subscription change landing between the entitled
    // read and the detached read could leave a skill in NEITHER list, which the client reads as an
    // upstream withdrawal — cleaning agent dirs for a skill the person still subscribes to. The
    // staleness window rides the same snapshot (one extra round trip, no serialization surface).
    let mut tx = authority.db().begin_delivery_snapshot().await?;
    let staleness_window_ms = authority.db().staleness_window(&mut tx, ws).await?;
    let entitled = authority
        .db()
        .entitled_skills(&mut tx, ws, &identity.principal, &identity.device_key_id)
        .await?;
    let detached = authority
        .db()
        .detached_skills(&mut tx, ws, &identity.principal, &identity.device_key_id)
        .await?;
    let excluded = authority
        .db()
        .device_exclusions(&mut tx, ws, &identity.device_key_id)
        .await?;
    let notice_rows = authority
        .db()
        .unacked_notices(&mut tx, ws, &identity.principal)
        .await?;
    // The snapshot has served its purpose; the read minted nothing durable, so the commit only
    // releases it (a rollback would be equally correct).
    tx.commit().await.map_err(AuthorityError::internal)?;
    // The open-proposal count folds to ONE aggregate over the entitled skill ids — the delivery hot
    // path fired the former per-skill `open_proposal_rows` loop once per entitled skill (the N+1).
    // Deliberately OUTSIDE the snapshot: a disclosure gauge, not a semantic signal the client acts on
    // with bytes. The aggregate shares the `open ∧ base == current` staleness predicate verbatim (a
    // further tracked copy — see `Db::count_open_proposals`).
    let skill_ids: Vec<String> = entitled.iter().map(|r| r.skill_id.clone()).collect();
    let proposals_awaiting = authority.db().count_open_proposals(ws, &skill_ids).await?;
    let skills = entitled
        .into_iter()
        .map(|row| {
            let EntitledDbRow {
                skill_id,
                name,
                display_name,
                protection,
                commit,
                generation,
                updated_at,
                bundle_digest,
                via_channels,
                direct,
            } = row;
            DeliveredSkill {
                skill_id,
                name,
                display_name,
                protection,
                version_id: commit,
                generation,
                updated_at,
                bundle_digest,
                via_channels,
                direct,
            }
        })
        .collect();
    let notices = notice_rows
        .into_iter()
        .map(
            |NoticeDbRow {
                 id,
                 kind,
                 skill_id,
                 skill_name,
                 version_id,
                 actor,
                 outcome,
                 reason,
                 message,
                 created_at,
             }| DeliveryNotice {
                id,
                kind,
                skill_id,
                skill_name,
                version_id,
                actor,
                outcome,
                reason,
                message,
                created_at,
            },
        )
        .collect();
    Ok(Delivery {
        skills,
        detached,
        excluded,
        notices,
        proposals_awaiting,
        staleness_window_ms,
    })
}

pub(crate) async fn report_applied(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
    applied: &[AppliedSkill],
    now: i64,
) -> Result<()> {
    let credential_sha256 = topos_core::digest::sha256(credential.as_bytes());
    let identity = authority
        .db()
        .resolve_read_credential(ws, &credential_sha256)
        .await?
        .ok_or(AuthorityError::NotFound)?;
    if !authority.db().read_gate(ws, &identity.principal).await? {
        return Err(AuthorityError::NotFound);
    }
    let pairs: Vec<(SkillId, CommitId)> = applied
        .iter()
        .map(|a| (a.skill_id.clone(), a.version_id))
        .collect();
    authority
        .db()
        .report_applied_txn(
            ws,
            &identity.principal,
            &identity.device_key_id,
            &pairs,
            now,
        )
        .await
}
