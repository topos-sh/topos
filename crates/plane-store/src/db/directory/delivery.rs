//! The delivery + fleet SQL — the raw-`sqlx` half of "what should this device have" and the
//! applied-state report. A child of `mod db`; no `sqlx` type crosses the boundary.
//!
//! The entitlement computation is ONE SQL home: the `topos_entitled_skills` set-returning function
//! (0015), which extends the confirmed-membership predicate every lane gates on — this file only
//! reads it. The report write is a snapshot upsert that NEVER touches a detach record (the frozen
//! "last applied" the fleet page shows).

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
    /// The entitled set for `(principal, device)` — a pool read over the ONE entitlement function.
    /// The caller's membership gate has already run (the function's own membership join is
    /// defense-in-depth, not the front door).
    pub(crate) async fn entitled_skills(
        &self,
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
        .fetch_all(self.pool())
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
                        epoch: u64::try_from(r.epoch)
                            .map_err(AuthorityError::integrity)?,
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

    /// The skills this person DETACHED (freeze-in-place on every device): the unfollow masks ∪ the
    /// detach records across the person's devices — minus anything currently entitled again
    /// (entitlement wins: presence in the delivered set re-attaches). Skill ids, sorted.
    pub(crate) async fn detached_skills(
        &self,
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
                   SELECT st.skill_id FROM device_skill_state st
                   JOIN device_registry dr ON dr.workspace_id = st.workspace_id
                                          AND dr.device_key_id = st.device_key_id
                   WHERE st.workspace_id = $1 AND dr.principal = $2 AND st.detached = 1
               ) d
               WHERE NOT EXISTS (
                   SELECT 1 FROM topos_entitled_skills($1, $2, $3) e WHERE e.skill_id = d.skill_id
               )
               ORDER BY d.skill_id"#,
            ws_s,
            principal.as_str(),
            device_key_id,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(rows.into_iter().map(|r| r.skill_id).collect())
    }

    /// This person's unacked notices, oldest first (the hook fetches without acking; the ack write
    /// is a later surface).
    pub(crate) async fn unacked_notices(
        &self,
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
        .fetch_all(self.pool())
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
    /// the device's `last_report_at` (the dashboard's staleness clock). DETACH RECORDS ARE IMMUTABLE
    /// HERE: a detached row is neither updated nor deleted (it is the frozen "last applied" the
    /// fleet page shows); re-attach happens through the subscription reconciles, never a report.
    /// Rows for skill ids the catalog does not know are dropped (a report is client-asserted data).
    pub(crate) async fn report_applied_txn(
        &self,
        ws: &WorkspaceId,
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
            sqlx::query!(
                "INSERT INTO device_skill_state (workspace_id, device_key_id, skill_id, applied_commit, reported_at) \
                 SELECT $1, $2, r.skill_id, r.applied_commit, $3 \
                 FROM UNNEST($4::TEXT[], $5::BYTEA[]) AS r(skill_id, applied_commit) \
                 JOIN catalog cat ON cat.workspace_id = $1 AND cat.skill_id = r.skill_id \
                 ON CONFLICT (workspace_id, device_key_id, skill_id) DO UPDATE \
                   SET applied_commit = excluded.applied_commit, reported_at = excluded.reported_at \
                   WHERE device_skill_state.detached = 0",
                ws_s,
                device_key_id,
                now,
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
