//! The delivery + fleet SQL — the raw-`sqlx` half of "what should this device have" and the
//! applied-state report. A child of `mod db`; no `sqlx` type crosses the boundary.
//!
//! The entitlement computation is ONE SQL home: the `topos_entitled_skills` set-returning function
//! (0015), which extends the confirmed-membership predicate every lane gates on — this file only
//! reads it. The report write is a snapshot upsert that NEVER touches a detach record (the frozen
//! "last applied" the fleet page shows).

use sqlx::Postgres;

use crate::db::{Db, blob32};
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, Principal, SkillId, WorkspaceId};
use topos_types::Generation;

/// One entitled skill as the delivery read returns it — catalog identity + the `current` pointer
/// facts + the resolved protection + the `via` attribution.
pub(crate) struct EntitledDbRow {
    pub(crate) skill_id: String,
    pub(crate) name: String,
    pub(crate) display_name: Option<String>,
    /// `"open"` / `"reviewed"` — the resolved cascade (pin, else workspace default).
    pub(crate) protection: String,
    pub(crate) commit: [u8; 32],
    pub(crate) generation: Generation,
    pub(crate) updated_at: i64,
    pub(crate) bundle_digest: [u8; 32],
    /// The channels delivering it (names, sorted; `everyone` included).
    pub(crate) via_channels: Vec<String>,
    /// Whether a direct follow also delivers it.
    pub(crate) direct: bool,
}

/// One unacked person-scoped notice, joined with the skill's catalog name for narration.
pub(crate) struct NoticeDbRow {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) skill_id: Option<String>,
    pub(crate) skill_name: Option<String>,
    pub(crate) version_id: Option<[u8; 32]>,
    pub(crate) actor: Option<String>,
    pub(crate) outcome: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) created_at: String,
}

impl Db {
    /// Open the delivery read's ONE snapshot transaction. The entitled set, the detached set, and the
    /// notices MUST be read from a single consistent snapshot: a subscription change landing between
    /// two independent reads could leave a skill in NEITHER list, and the client reads
    /// "delivered nowhere, detached nowhere" as an UPSTREAM withdrawal — cleaning agent dirs for a
    /// skill the person still subscribes to. `REPEATABLE READ` is exactly the guarantee needed (the
    /// read mints nothing durable, so no serialization retry is required).
    pub(crate) async fn begin_delivery_snapshot(&self) -> Result<sqlx::Transaction<'_, Postgres>> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(AuthorityError::internal)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await
            .map_err(AuthorityError::internal)?;
        Ok(tx)
    }

    /// The entitled set for `(principal, device)` — read over the ONE entitlement function.
    /// The caller's membership gate has already run (the function's own membership join is
    /// defense-in-depth, not the front door).
    pub(crate) async fn entitled_skills(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        principal: &Principal,
        device_key_id: &str,
    ) -> Result<Vec<EntitledDbRow>> {
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"SELECT skill_id AS "skill_id!", name AS "name!", display_name AS "display_name?",
                      protection AS "protection!", commit_id AS "commit_id!: Vec<u8>",
                      epoch AS "epoch!: i64", seq AS "seq!: i64", updated_at AS "updated_at!: i64",
                      bundle_digest AS "bundle_digest?: Vec<u8>",
                      via_channels AS "via_channels!: Vec<String>", direct AS "direct!: i64"
               FROM topos_entitled_skills($1, $2, $3)"#,
            ws_s,
            principal.as_str(),
            device_key_id,
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                Ok(EntitledDbRow {
                    skill_id: r.skill_id,
                    name: r.name,
                    display_name: r.display_name,
                    protection: r.protection,
                    commit: blob32(&r.commit_id)?,
                    generation: Generation {
                        epoch: u64::try_from(r.epoch).map_err(AuthorityError::integrity)?,
                        seq: u64::try_from(r.seq).map_err(AuthorityError::integrity)?,
                    },
                    updated_at: r.updated_at,
                    bundle_digest: blob32(
                        &r.bundle_digest
                            .ok_or_else(|| AuthorityError::integrity(MissingDeliveryDigest))?,
                    )?,
                    via_channels: r.via_channels,
                    direct: r.direct != 0,
                })
            })
            .collect()
    }

    /// The skills this person DETACHED — the who-acts signal every device freezes on: the
    /// person-scoped DETACHMENT RECORDS (written by their own unfollow / channel leave / membership
    /// removal, scoped to exactly what that event lapsed) ∪ their standing unfollow masks, minus
    /// anything currently entitled again (entitlement always wins — a re-follow, a curator's
    /// re-placement, an unarchive all revive delivery). Skill ids, sorted.
    ///
    /// PERSON-scoped by construction: it never consults `device_skill_state`, so a device that has
    /// never reported still learns the person detached a skill (a fleet row need not exist), and a
    /// skill an UPSTREAM act removed — an unplace, an archive — is deliberately absent here, which
    /// is what tells the client to CLEAN rather than freeze.
    pub(crate) async fn detached_skills(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        principal: &Principal,
        device_key_id: &str,
    ) -> Result<Vec<String>> {
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"SELECT DISTINCT d.skill_id AS "skill_id!"
               FROM (
                   SELECT u.skill_id FROM skill_unfollows u
                   WHERE u.workspace_id = $1 AND u.principal = $2
                   UNION
                   SELECT dt.skill_id FROM skill_detachments dt
                   WHERE dt.workspace_id = $1 AND dt.principal = $2
               ) d
               WHERE NOT EXISTS (
                   SELECT 1 FROM topos_entitled_skills($1, $2, $3) e WHERE e.skill_id = d.skill_id
               )
               ORDER BY d.skill_id"#,
            ws_s,
            principal.as_str(),
            device_key_id,
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(rows.into_iter().map(|r| r.skill_id).collect())
    }

    /// This person's unacked notices, oldest first (the hook fetches without acking; the ack write
    /// is a later surface).
    pub(crate) async fn unacked_notices(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        principal: &Principal,
    ) -> Result<Vec<NoticeDbRow>> {
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"SELECT n.id AS "id!", n.kind AS "kind!", n.skill_id AS "skill_id?",
                      cat.name AS "skill_name?", n.version_id AS "version_id?: Vec<u8>",
                      n.actor AS "actor?", n.outcome AS "outcome?", n.reason AS "reason?",
                      n.message AS "message?", n.created_at AS "created_at!"
               FROM notices n
               LEFT JOIN catalog cat ON cat.workspace_id = n.workspace_id AND cat.skill_id = n.skill_id
               WHERE n.workspace_id = $1 AND n.principal = $2 AND n.acked_at IS NULL
               ORDER BY n.created_at, n.id"#,
            ws_s,
            principal.as_str(),
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                Ok(NoticeDbRow {
                    id: r.id,
                    kind: r.kind,
                    skill_id: r.skill_id,
                    skill_name: r.skill_name,
                    version_id: r.version_id.as_deref().map(blob32).transpose()?,
                    actor: r.actor,
                    outcome: r.outcome,
                    reason: r.reason,
                    message: r.message,
                    created_at: r.created_at,
                })
            })
            .collect()
    }

    /// The applied-state report: ONE snapshot upsert per device — refresh the reported rows, drop
    /// the non-detached rows the snapshot no longer names (the device no longer holds them), stamp
    /// the device's `last_report_at` (the dashboard's staleness clock). Rows for skill ids the
    /// catalog does not know are dropped (a report is client-asserted data).
    ///
    /// Every reported skill is re-checked against the SERVER's entitlement predicate — a report is
    /// client-asserted data, and the plane records only what it actually delivers to this (person,
    /// device). An ENTITLED reported skill revives its row (`detached = 0`), which is what heals a
    /// fleet row a lapse froze before a curator re-placed the skill; a DETACHED skill is by
    /// definition not entitled, so no client can revive a detach record the plane is deliberately
    /// holding, and a frozen row stays as the final "last known state" the fleet page names as its
    /// blind spot.
    pub(crate) async fn report_applied_txn(
        &self,
        ws: &WorkspaceId,
        principal: &Principal,
        device_key_id: &str,
        applied: &[(SkillId, CommitId)],
        now: i64,
    ) -> Result<()> {
        let ws_s = ws.as_str();
        let skill_ids: Vec<String> = applied.iter().map(|(s, _)| s.as_str().to_owned()).collect();
        let commits: Vec<Vec<u8>> = applied.iter().map(|(_, c)| c.0.to_vec()).collect();
        run_serializable!(self, tx, {
            sqlx::query!(
                "UPDATE device_registry SET last_report_at = $3 \
                 WHERE workspace_id = $1 AND device_key_id = $2",
                ws_s,
                device_key_id,
                now,
            )
            .execute(&mut *tx)
            .await
            .map_err(AuthorityError::internal)?;
            // The report is CLIENT-ASSERTED data, so the server decides what may be recorded: only
            // skills the delivery predicate ACTUALLY DELIVERS to this (person, device) survive the
            // join. That is what makes the `detached = 0` revive safe — a detached skill is by
            // definition NOT entitled (the detachment stands until an entitlement heals it), so no
            // client can revive a detach record the plane is deliberately holding, nor record a
            // skill it was never entitled to.
            sqlx::query!(
                "INSERT INTO device_skill_state (workspace_id, device_key_id, skill_id, applied_commit, reported_at) \
                 SELECT $1, $2, r.skill_id, r.applied_commit, $3 \
                 FROM UNNEST($5::TEXT[], $6::BYTEA[]) AS r(skill_id, applied_commit) \
                 JOIN topos_entitled_skills($1, $4, $2) e ON e.skill_id = r.skill_id \
                 ON CONFLICT (workspace_id, device_key_id, skill_id) DO UPDATE \
                   SET applied_commit = excluded.applied_commit, reported_at = excluded.reported_at, \
                       detached = 0, detached_at = NULL",
                ws_s,
                device_key_id,
                now,
                principal.as_str(),
                &skill_ids,
                &commits,
            )
            .execute(&mut *tx)
            .await
            .map_err(AuthorityError::internal)?;
            sqlx::query!(
                "DELETE FROM device_skill_state \
                 WHERE workspace_id = $1 AND device_key_id = $2 AND detached = 0 \
                   AND skill_id <> ALL($3::TEXT[])",
                ws_s,
                device_key_id,
                &skill_ids,
            )
            .execute(&mut *tx)
            .await
            .map_err(AuthorityError::internal)?;
            Ok(())
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("an entitled skill's provenance row carries no bundle_digest")]
struct MissingDeliveryDigest;
