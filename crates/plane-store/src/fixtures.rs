//! The feature-gated test-fixtures shims — NEVER part of the production API.
//!
//! Split out of [`crate::authority`] so that file reads as exactly the production surface. A clearly-marked,
//! feature-gated `impl Authority` a DOWNSTREAM test crate (the OSS plane's HTTP routes, the HERO loopback)
//! drives to stage an authority without a real enrollment subsystem. Each shim only DRIVES an existing op or
//! seed helper — it grants no capability the production API doesn't already enforce (a write still needs a
//! registered, non-revoked, rostered device; a read still needs a minted token). The whole module is gated
//! behind `feature = "test-fixtures"`, which the production `topos-plane` build never enables (a CI guard
//! asserts it), so none of this ships.

use crate::authority::Authority;
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, OpId, Principal, SkillId, WorkspaceId};
use crate::set_current::{DeviceOp, DeviceOpRequest, SetCurrentReceipt};

impl Authority {
    /// Stage a roster membership (the read/write entitlement for a principal on a skill). Test-only.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn seed_roster(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
    ) -> Result<()> {
        self.db().seed_roster(ws, skill, principal).await
    }

    /// Register a device key — `(device_key_id) -> (public_key, principal, revoked)` — the pointer-move's
    /// in-transaction authorization resolves against. Test-only (real issuance is the enrollment port's).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn seed_device(
        &self,
        ws: &WorkspaceId,
        device_key_id: &str,
        public_key: &[u8; 32],
        principal: &Principal,
        revoked: bool,
    ) -> Result<()> {
        self.db()
            .seed_device(ws, device_key_id, public_key, principal, revoked)
            .await
    }

    /// Set the workspace's `review_required` policy (the anti-poisoning gate). Test-only convenience that
    /// **delegates** to the public [`set_review_required`](Self::set_review_required) (one impl, no drift) —
    /// kept so the existing fixtures read the same way; a downstream plane uses the public op.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn seed_review_required(
        &self,
        ws: &WorkspaceId,
        review_required: bool,
    ) -> Result<()> {
        self.set_review_required(ws, review_required).await
    }

    /// Mint a read token (store only its sha256, exactly as [`resolve_read_token`](Self::resolve_read_token)
    /// looks it up). Test-only — the real minting + the `0600` at-rest token file land with the enrollment port.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn mint_read_token(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
        token: &str,
    ) -> Result<()> {
        self.db().seed_read_token(ws, skill, principal, token).await
    }

    /// Stage a `workspace` row (the enrollment/governance billable object) so a downstream cloud-enrollment
    /// or governance test can stand up a workspace without the cloud product's provisioning. Test-only.
    /// `verified_domain_status` ∈ {`unverified`,`pending`,`verified`}; `deployment_mode` ∈ {`cloud`,`self_host`}.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn seed_workspace(
        &self,
        ws: &WorkspaceId,
        display_name: &str,
        verified_domain_status: &str,
        deployment_mode: &str,
    ) -> Result<()> {
        self.db()
            .seed_workspace(ws, display_name, verified_domain_status, deployment_mode)
            .await
    }

    /// Stage a `workspace_member` row (the workspace RBAC roster) so a downstream test can seat an owner
    /// without the enrollment path. Test-only. `role` ∈ {`owner`,`reviewer`,`member`}; `status` ∈
    /// {`invited`,`confirmed`}.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn seed_workspace_member(
        &self,
        ws: &WorkspaceId,
        principal: &Principal,
        role: &str,
        status: &str,
    ) -> Result<()> {
        self.db()
            .seed_workspace_member(ws, principal, role, status)
            .await
    }

    /// Drive a REAL genesis [`publish`](Self::publish) for a registered + rostered device — producing a
    /// `current` pointer at generation (1,1). The device must already be registered
    /// ([`seed_device`](Self::seed_device)) + rostered ([`seed_roster`](Self::seed_roster)). Test-only.
    ///
    /// Returns the durable [`SetCurrentReceipt`] (its `version_id`/`current` drive a follow-up test).
    ///
    /// # Errors
    /// As [`publish`](Self::publish); [`AuthorityError::RejectedUpload`] if the candidate is malformed.
    #[allow(clippy::too_many_arguments)]
    pub async fn seed_published_genesis(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        device_key_id: &str,
        op_id: &OpId,
        files: Vec<crate::UploadedFile>,
        author: &str,
        message: &str,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        let device = DeviceOpRequest {
            device_key_id: device_key_id.to_owned(),
            op: DeviceOp::PublishDirect,
            expected: topos_types::Generation { epoch: 0, seq: 0 },
        };
        let candidate = crate::CandidateUpload {
            files,
            parents: vec![],
            author: author.to_owned(),
            message: message.to_owned(),
        };
        self.publish(ws, skill, op_id, candidate, device, None, created_at, now)
            .await
    }

    /// Drive a REAL one-parent forward [`publish`](Self::publish) on top of `parent` (mirrors
    /// [`seed_published_genesis`](Self::seed_published_genesis), but a child move rather than genesis), so a
    /// test can advance `current` to a v2. The expected base generation is read from the skill's live
    /// `current` (so a child right after the genesis seed bases on `(1,1)`) and the publish runs through
    /// the same CAS/availability/lineage/receipt backbone. Test-only.
    ///
    /// # Errors
    /// As [`publish`](Self::publish); [`AuthorityError::RejectedUpload`] if the candidate is malformed or the
    /// skill has no `current` to base a child on.
    #[allow(clippy::too_many_arguments)]
    pub async fn seed_published_child(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        device_key_id: &str,
        op_id: &OpId,
        parent: CommitId,
        files: Vec<crate::UploadedFile>,
        author: &str,
        message: &str,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        // The base generation a child must match is whatever `current` is right now (after a genesis seed,
        // `(1,1)`) — read it from the live record so the CAS in `publish` accepts the move.
        let record_bytes = self
            .db()
            .read_current_record(ws, skill)
            .await?
            .ok_or_else(|| {
                AuthorityError::RejectedUpload("no current to base a child on".to_owned())
            })?;
        let record: topos_types::WireCurrentRecord =
            serde_json::from_slice(&record_bytes).map_err(AuthorityError::internal)?;
        let expected = record.record.generation;

        let device = DeviceOpRequest {
            device_key_id: device_key_id.to_owned(),
            op: DeviceOp::PublishDirect,
            expected,
        };
        let candidate = crate::CandidateUpload {
            files,
            parents: vec![parent],
            author: author.to_owned(),
            message: message.to_owned(),
        };
        self.publish(ws, skill, op_id, candidate, device, None, created_at, now)
            .await
    }

    /// Overwrite the skill's stored `current` record with arbitrary bytes — drives the corrupt-stored-blob
    /// integrity path (an unparseable record is an Integrity fault, never a not-found). Test-only.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn force_current_record(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        bytes: &[u8],
    ) -> Result<()> {
        self.db().force_current_record(ws, skill, bytes).await
    }
}
