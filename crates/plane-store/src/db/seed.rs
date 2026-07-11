//! Test-only staging helpers — `pub(crate)`, **never public.**
//!
//! These insert roster / provenance / pointer rows directly so the access-port, lineage, and
//! isolation tests can stage state without the (deferred) pointer-move write. They are never `pub` —
//! a public seed would let any in-process linker grant itself read entitlement, the exact hole the
//! privacy wall closes. The module is gated under `cfg(any(test, feature = "test-fixtures"))`: `test`
//! keeps them out of every release artifact, while the feature exposes the small subset the
//! `Authority` test-fixtures shims drive (the rest stay `pub(crate)` staging helpers the in-crate tests
//! use, so they are legitimately dead in a feature-only — non-test — build).
#![cfg_attr(not(test), allow(dead_code))]

use sqlx::{Postgres, Transaction};
use topos_core::digest;

use super::Db;
use crate::db::custody::lifecycle::GIT_OID_LEN;
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, Principal, SkillId, WorkspaceId};

impl Db {
    /// Stage a `deleting` object_presence row directly (a crashed GC's leftover) — drives the recovery sweep.
    pub(crate) async fn seed_deleting_object(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
        git_oid: &[u8; GIT_OID_LEN],
        status_updated_at: i64,
    ) -> Result<()> {
        let ws_s = ws.as_str();
        let oid = object_id.0.as_slice();
        let goid = git_oid.as_slice();
        sqlx::query!(
            "INSERT INTO object_presence (workspace_id, object_id, status, location, size, git_oid, status_updated_at) \
             VALUES ($1, $2, 'deleting', 'git', 0, $3, $4) \
             ON CONFLICT (workspace_id, object_id) DO UPDATE SET status='deleting', git_oid=excluded.git_oid, \
               status_updated_at=excluded.status_updated_at",
            ws_s,
            oid,
            goid,
            status_updated_at,
        )
        .execute(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }
    /// Stage a roster membership (the principal becomes entitled to read/upload the skill).
    pub(crate) async fn seed_roster(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
    ) -> Result<()> {
        let (ws, skill, principal) = (ws.as_str(), skill.as_str(), principal.as_str());
        sqlx::query!(
            "INSERT INTO roster (workspace_id, skill_id, principal) VALUES ($1, $2, $3) \
             ON CONFLICT (workspace_id, skill_id, principal) DO NOTHING",
            ws,
            skill,
            principal,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Stage a commit's provenance (skill ownership) + reachability edges directly, with no git write —
    /// for access-join, lineage, and isolation tests that exercise the database logic in isolation. The
    /// primary key still enforces single-skill ownership, so this cannot stage a cross-skill commit.
    pub(crate) async fn seed_commit(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        commit: CommitId,
        objects: &[ObjectId],
    ) -> Result<()> {
        run_serializable!(
            self,
            tx,
            seed_commit_txn(&mut tx, ws, skill, commit, objects).await
        )
    }

    /// Stage a proposal row directly (test-only), inserting the candidate's `skill_commit` provenance (the
    /// foreign-key target) if absent. Drives the GC-retention + read-authorization proposal-arm tests without
    /// the (separately exercised) propose write path. `base_commit`/`base_(epoch,seq)` set the proposal's
    /// base; pair with a `current` row at that generation to make it non-stale, or a later one to stale it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn seed_proposal(
        &self,
        ws: &WorkspaceId,
        id: &str,
        skill: &SkillId,
        commit: CommitId,
        base_commit: CommitId,
        base_epoch: i64,
        base_seq: i64,
        status: &str,
        proposer: &Principal,
    ) -> Result<()> {
        run_serializable!(self, tx, {
            seed_proposal_txn(
                &mut tx,
                ws,
                id,
                skill,
                commit,
                base_commit,
                base_epoch,
                base_seq,
                status,
                proposer,
            )
            .await
        })
    }

    /// Stage a proposal's object root directly (the gated retention/read root for a pending proposal).
    pub(crate) async fn seed_proposal_object(
        &self,
        ws: &WorkspaceId,
        proposal_id: &str,
        object_id: ObjectId,
    ) -> Result<()> {
        let ws_s = ws.as_str();
        let oid = object_id.0.as_slice();
        sqlx::query!(
            "INSERT INTO proposal_object (workspace_id, proposal_id, object_id) VALUES ($1, $2, $3) \
             ON CONFLICT (workspace_id, proposal_id, object_id) DO NOTHING",
            ws_s,
            proposal_id,
            oid,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Remove a roster membership — the revocation mechanism (membership = a row exists, so revocation
    /// is row deletion). Test-only here; enrollment owns issuance + revocation later.
    pub(crate) async fn delete_roster(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
    ) -> Result<()> {
        let (ws, skill, principal) = (ws.as_str(), skill.as_str(), principal.as_str());
        sqlx::query!(
            "DELETE FROM roster WHERE workspace_id = $1 AND skill_id = $2 AND principal = $3",
            ws,
            skill,
            principal,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Register a fixture device — `(device_key_id) -> (public_key, principal, revoked)` — the pointer-move's
    /// in-transaction authorization resolves against. Real issuance lands later behind the enrollment port.
    pub(crate) async fn seed_device(
        &self,
        ws: &WorkspaceId,
        device_key_id: &str,
        public_key: &[u8; 32],
        principal: &Principal,
        revoked: bool,
    ) -> Result<()> {
        let ws_s = ws.as_str();
        let pk = public_key.as_slice();
        let principal_s = principal.as_str();
        let revoked_i = i64::from(revoked);
        sqlx::query!(
            "INSERT INTO device_registry (workspace_id, device_key_id, public_key, principal, revoked) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (workspace_id, device_key_id) DO UPDATE SET \
               public_key = excluded.public_key, principal = excluded.principal, revoked = excluded.revoked",
            ws_s,
            device_key_id,
            pk,
            principal_s,
            revoked_i,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Revoke a registered device (the device-side revocation: `revoked = 1`). A revoke committed before a
    /// promotion is serialized ahead of the pointer-move's in-transaction device read and blocks the move.
    pub(crate) async fn revoke_device(&self, ws: &WorkspaceId, device_key_id: &str) -> Result<()> {
        let ws_s = ws.as_str();
        sqlx::query!(
            "UPDATE device_registry SET revoked = 1 WHERE workspace_id = $1 AND device_key_id = $2",
            ws_s,
            device_key_id,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Force a skill's `current` generation (test-only) — simulates a backup/restore that bumps `epoch`
    /// while reusing `seq`, so the restore-ABA test can prove the CAS compares the WHOLE `(epoch, seq)` pair
    /// (a seq-only CAS would wrongly accept a stale base at the reused `seq`).
    pub(crate) async fn force_current_generation(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        epoch: i64,
        seq: i64,
    ) -> Result<()> {
        let ws_s = ws.as_str();
        let skill_s = skill.as_str();
        sqlx::query!(
            "UPDATE current SET epoch = $3, seq = $4 WHERE workspace_id = $1 AND skill_id = $2",
            ws_s,
            skill_s,
            epoch,
            seq,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Stage a read token (the per-follower, per-skill read credential) — storing only its sha256, never the
    /// plaintext, exactly as the resolver looks it up. Real minting (and the 0600 at-rest token file) lands
    /// later behind the enrollment port.
    pub(crate) async fn seed_read_token(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
        token: &str,
    ) -> Result<()> {
        let (ws_s, skill_s, principal_s) = (ws.as_str(), skill.as_str(), principal.as_str());
        let token_sha256 = digest::sha256(token.as_bytes());
        let key = token_sha256.as_slice();
        sqlx::query!(
            "INSERT INTO read_token (workspace_id, skill_id, principal, token_sha256) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (token_sha256) DO UPDATE SET \
               workspace_id = excluded.workspace_id, skill_id = excluded.skill_id, principal = excluded.principal",
            ws_s,
            skill_s,
            principal_s,
            key,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Overwrite a skill's `current.signed_record` with arbitrary bytes (test-only) — drives the
    /// `read_current` corrupt-blob path (an unparseable stored record is an Integrity fault, never not-found).
    /// Requires the pointer to exist first.
    pub(crate) async fn force_signed_record(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        bytes: &[u8],
    ) -> Result<()> {
        let (ws_s, skill_s) = (ws.as_str(), skill.as_str());
        sqlx::query!(
            "UPDATE current SET signed_record = $3 WHERE workspace_id = $1 AND skill_id = $2",
            ws_s,
            skill_s,
            bytes,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Stage a `workspace` row (the enrollment/governance billable object) — so cloud enrollment + governance
    /// tests can stand up a workspace without the (deferred) admin-claim/provisioning path. Test-only; real
    /// workspace creation is the cloud product's / `admin_claim`'s job. `created_at` is a fixed `'seed'`.
    pub(crate) async fn seed_workspace(
        &self,
        ws: &WorkspaceId,
        display_name: &str,
        verified_domain_status: &str,
        deployment_mode: &str,
    ) -> Result<()> {
        let ws_s = ws.as_str();
        sqlx::query!(
            "INSERT INTO workspace (workspace_id, display_name, verified_domain, verified_domain_status, deployment_mode, created_at) \
             VALUES ($1, $2, NULL, $3, $4, 'seed') \
             ON CONFLICT (workspace_id) DO UPDATE SET \
               display_name = excluded.display_name, verified_domain_status = excluded.verified_domain_status, \
               deployment_mode = excluded.deployment_mode",
            ws_s,
            display_name,
            verified_domain_status,
            deployment_mode,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Stage a `workspace_member` row (the workspace RBAC roster) — so governance tests can seat an owner
    /// without the enrollment path. Test-only. `added_at` is a fixed `'seed'`.
    pub(crate) async fn seed_workspace_member(
        &self,
        ws: &WorkspaceId,
        principal: &Principal,
        role: &str,
        status: &str,
    ) -> Result<()> {
        let (ws_s, prin) = (ws.as_str(), principal.as_str());
        sqlx::query!(
            "INSERT INTO workspace_member (workspace_id, principal, role, status, invited_by, added_at) \
             VALUES ($1, $2, $3, $4, NULL, 'seed') \
             ON CONFLICT (workspace_id, principal) DO UPDATE SET role = excluded.role, status = excluded.status",
            ws_s,
            prin,
            role,
            status,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Stage a one-time `admin_claim` token (storing only its sha256) — so the self-host first-boot
    /// `admin_claim` op can be driven in a test. Test-only; the real provisioner mints + logs the plaintext.
    pub(crate) async fn seed_admin_claim(&self, ws: &WorkspaceId, token: &str) -> Result<()> {
        let token_sha256 = digest::sha256(token.as_bytes());
        let (ws_s, key) = (ws.as_str(), token_sha256.as_slice());
        sqlx::query!(
            "INSERT INTO admin_claim (token_sha256, workspace_id, consumed_at, created_at) \
             VALUES ($1, $2, NULL, 'seed') ON CONFLICT (token_sha256) DO NOTHING",
            key,
            ws_s,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Stage the per-skill `current` pointer (created, never moved this increment; the signed record
    /// stays absent). Requires the commit's provenance to exist first (the foreign key).
    pub(crate) async fn seed_current(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        commit: CommitId,
        epoch: i64,
        seq: i64,
    ) -> Result<()> {
        let (ws_s, skill_s) = (ws.as_str(), skill.as_str());
        let cid = commit.0.as_slice();
        sqlx::query!(
            "INSERT INTO current (workspace_id, skill_id, commit_id, epoch, seq, signed_record, updated_at) \
             VALUES ($1, $2, $3, $4, $5, NULL, 0) \
             ON CONFLICT (workspace_id, skill_id) DO NOTHING",
            ws_s,
            skill_s,
            cid,
            epoch,
            seq,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }
}

/// The body of [`Db::seed_commit`], factored out so the serializable runner can re-run it on a retry: it
/// borrows its inputs (never consumes them) and touches only the transaction, so a retry is byte-identical.
/// Stages a commit's provenance (skill ownership) + reachability edges directly, with no git write.
async fn seed_commit_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    skill: &SkillId,
    commit: CommitId,
    objects: &[ObjectId],
) -> Result<()> {
    let (ws_s, skill_s) = (ws.as_str(), skill.as_str());
    let cid = commit.0.as_slice();
    sqlx::query!(
        "INSERT INTO skill_commit (workspace_id, commit_id, skill_id) VALUES ($1, $2, $3) \
         ON CONFLICT (workspace_id, commit_id) DO NOTHING",
        ws_s,
        cid,
        skill_s,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    for obj in objects {
        let object = obj.0.as_slice();
        sqlx::query!(
            "INSERT INTO commit_object (workspace_id, commit_id, object_id) VALUES ($1, $2, $3) \
             ON CONFLICT (workspace_id, commit_id, object_id) DO NOTHING",
            ws_s,
            cid,
            object,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
    }
    Ok(())
}

/// The body of [`Db::seed_proposal`], factored out so the serializable runner can re-run it on a retry: it
/// borrows its inputs (never consumes them) and touches only the transaction, so a retry is byte-identical.
/// Stages a proposal row directly, inserting the candidate's `skill_commit` provenance (the foreign-key
/// target) if absent.
#[allow(clippy::too_many_arguments)]
async fn seed_proposal_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    id: &str,
    skill: &SkillId,
    commit: CommitId,
    base_commit: CommitId,
    base_epoch: i64,
    base_seq: i64,
    status: &str,
    proposer: &Principal,
) -> Result<()> {
    let (ws_s, skill_s) = (ws.as_str(), skill.as_str());
    let cid = commit.0.as_slice();
    let base_cid = base_commit.0.as_slice();
    let proposer_s = proposer.as_str();
    sqlx::query!(
        "INSERT INTO skill_commit (workspace_id, commit_id, skill_id) VALUES ($1, $2, $3) \
         ON CONFLICT (workspace_id, commit_id) DO NOTHING",
        ws_s,
        cid,
        skill_s,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    sqlx::query!(
        "INSERT INTO proposals \
           (workspace_id, id, skill_id, commit_id, base_commit_id, base_epoch, base_seq, status, proposer, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'seed')",
        ws_s,
        id,
        skill_s,
        cid,
        base_cid,
        base_epoch,
        base_seq,
        status,
        proposer_s,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}
