//! The session-read lane's one new query — the per-workspace skill index. A child of `mod db`; no
//! `sqlx` type crosses the boundary. Everything else the session lane reads goes through the existing
//! reachability statements (`object_witness` / `version_readable` / `open_proposal_rows`), re-gated by
//! [`crate::db::ReadLane::WorkspaceMember`] — this file exists only for the index join no token-scoped read
//! ever needed.

use topos_types::Generation;

use crate::db::{Db, blob32};
use crate::error::{AuthorityError, Result};
use crate::id::WorkspaceId;

/// One row of the skill index as stored: the skill, its `current` pointer facts, and the pointed
/// version's provenance digest. The proposal count is NOT here — the orchestration delegates it
/// per-skill to `open_proposal_rows` so the staleness predicate stays in its one listing home.
pub(crate) struct SkillIndexDbRow {
    pub(crate) skill_id: String,
    pub(crate) commit: [u8; 32],
    pub(crate) generation: Generation,
    pub(crate) updated_at: i64,
    pub(crate) bundle_digest: [u8; 32],
    /// The skill's UNSIGNED advisory display name (`current.display_name`), or `None` (the reader falls
    /// back to the skill id). Never part of the digest or any signature.
    pub(crate) display_name: Option<String>,
}

impl Db {
    /// Every skill in the workspace holding a `current` row, with its pointer generation, update time
    /// (epoch-milliseconds, the server clock unit), and the pointed version's `bundle_digest` — ordered
    /// by `skill_id` for a stable enumeration. Principal-free (the caller's member gate has already run;
    /// an autocommit pool read, so a publish committed elsewhere is visible to the next call).
    ///
    /// `skill_commit.bundle_digest` is NULLABLE in the schema (rows can predate the digest column), but a
    /// `current`-pointed version always recorded one — a NULL here is a provenance divergence, mapped to
    /// [`AuthorityError::Integrity`] (the same convention as the version-metadata read), never a not-found.
    pub(crate) async fn list_skill_index(&self, ws: &WorkspaceId) -> Result<Vec<SkillIndexDbRow>> {
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"
            SELECT c.skill_id      AS "skill_id!",
                   c.commit_id     AS "commit_id!: Vec<u8>",
                   c.epoch         AS "epoch!: i64",
                   c.seq           AS "seq!: i64",
                   c.updated_at    AS "updated_at!: i64",
                   c.display_name  AS "display_name?",
                   sc.bundle_digest AS "bundle_digest?: Vec<u8>"
            FROM current c
            JOIN skill_commit sc ON sc.workspace_id = c.workspace_id AND sc.commit_id = c.commit_id
            WHERE c.workspace_id = $1
            ORDER BY c.skill_id
            "#,
            ws_s,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                Ok(SkillIndexDbRow {
                    skill_id: r.skill_id,
                    commit: blob32(&r.commit_id)?,
                    generation: Generation {
                        epoch: i64_to_u64(r.epoch)?,
                        seq: i64_to_u64(r.seq)?,
                    },
                    updated_at: r.updated_at,
                    bundle_digest: blob32(
                        &r.bundle_digest
                            .ok_or_else(|| AuthorityError::integrity(MissingIndexDigest))?,
                    )?,
                    display_name: r.display_name,
                })
            })
            .collect()
    }
}

/// A stored generation component must fit `u64` (the schema stores non-negative BIGINTs); a negative
/// value is store corruption.
fn i64_to_u64(v: i64) -> Result<u64> {
    u64::try_from(v).map_err(AuthorityError::integrity)
}

#[derive(Debug, thiserror::Error)]
#[error("a current-pointed version's provenance row carries no bundle_digest")]
struct MissingIndexDigest;
