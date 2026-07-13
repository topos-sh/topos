//! The delivery + fleet SQL — since the door cutover, both are ONE guarded SQL function each
//! (`topos_delivery` / `topos_report_applied`, migration 0019): the web tier calls them directly
//! under its scoped role, and this crate calls the SAME functions through the thin statements
//! below — one implementation, whichever tier asks. A child of `mod db`; no `sqlx` type crosses
//! the boundary.
//!
//! What used to be Rust orchestration is now the functions' own guarantees: `topos_delivery` is a
//! single statement (one snapshot — the entitled/detached/notices sets can never straddle a
//! subscription change), and `topos_report_applied` fences itself `FOR UPDATE` on the device's
//! registry row (its other caller is a READ COMMITTED autocommit connection).

use crate::db::Db;
use crate::error::{AuthorityError, Result};
use crate::id::{Principal, WorkspaceId};

impl Db {
    /// The complete `WireDelivery` body from `topos_delivery`, or `None` when the function's own
    /// membership gate refuses (the caller has already run the front door; a `None` here is a
    /// revoke/removal racing the read — folded to the uniform not-found).
    pub(crate) async fn delivery_body(
        &self,
        ws: &WorkspaceId,
        principal: &Principal,
        device_key_id: &str,
    ) -> Result<Option<serde_json::Value>> {
        let body = sqlx::query_scalar!(
            r#"SELECT topos_delivery($1, $2, $3) AS "body?: serde_json::Value""#,
            ws.as_str(),
            principal.as_str(),
            device_key_id,
        )
        .fetch_one(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(body)
    }

    /// The applied-state report through `topos_report_applied` — `true` on 'ok', `false` when the
    /// function's own gate refused (same racing-revoke fold as the delivery read).
    pub(crate) async fn report_applied_fn(
        &self,
        ws: &WorkspaceId,
        principal: &Principal,
        device_key_id: &str,
        skill_ids: &[String],
        commits: &[Vec<u8>],
        now: i64,
    ) -> Result<bool> {
        let outcome = sqlx::query_scalar!(
            r#"SELECT topos_report_applied($1, $2, $3, $4, $5::TEXT[], $6::BYTEA[]) AS "outcome?""#,
            ws.as_str(),
            principal.as_str(),
            device_key_id,
            now,
            skill_ids,
            commits,
        )
        .fetch_one(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(outcome.as_deref() == Some("ok"))
    }
}
