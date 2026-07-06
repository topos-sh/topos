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
use crate::set_current::{DeviceSignedOp, SetCurrentReceipt};

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

    /// Drive a REAL genesis [`publish`](Self::publish): recompute the server-trusted ids the publish's ingest
    /// will derive (so the device op signs over them, exactly as an honest client would), sign with the given
    /// device seed, then publish — producing a SIGNED `current` pointer at generation (1,1). The device must
    /// already be registered ([`seed_device`](Self::seed_device)) + rostered ([`seed_roster`](Self::seed_roster)).
    /// Test-only.
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
        device_seed: &[u8; 32],
        op_id: &OpId,
        files: Vec<crate::UploadedFile>,
        author: &str,
        message: &str,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        use ed25519_dalek::{Signer as _, SigningKey};
        use topos_core::digest::{self, ManifestEntry};
        use topos_core::sign::{self, Commit, DeviceOp, DeviceOpFields, device_op_preimage};

        // The server-trusted genesis ids — identical to what `publish`'s ingest recomputes (both run the
        // kernel digest over the same `(path, mode, sha256(bytes))` manifest, with `parents = []`), so the
        // device op below signs over exactly what the in-transaction authz reconstructs.
        let manifest: Vec<ManifestEntry> = files
            .iter()
            .map(|f| ManifestEntry {
                path: f.path.clone(),
                mode: f.mode,
                content_sha256: digest::sha256(&f.bytes),
            })
            .collect();
        let bundle_digest = digest::bundle_digest(&manifest)
            .map_err(|r| AuthorityError::RejectedUpload(format!("{r:?}")))?;
        let version_id = sign::commit_id(&Commit {
            parents: &[],
            tree: bundle_digest,
            author,
            message,
        })
        .map_err(|e| AuthorityError::RejectedUpload(format!("{e:?}")))?;

        // Sign the device op over those ids at the genesis base (0,0).
        let op_id_bytes = uuid::Uuid::parse_str(op_id.as_str())
            .map_err(|_| {
                AuthorityError::RejectedUpload("op_id is not a canonical UUID".to_owned())
            })?
            .into_bytes();
        let fields = DeviceOpFields {
            workspace_id: ws.as_str(),
            skill_id: skill.as_str(),
            op: DeviceOp::PublishDirect,
            op_id: op_id_bytes,
            device_key_id,
            expected_epoch: 0,
            expected_seq: 0,
            commit_id: version_id,
            bundle_digest,
        };
        let preimage = device_op_preimage(&fields)
            .map_err(|e| AuthorityError::RejectedUpload(format!("{e:?}")))?;
        let signature = SigningKey::from_bytes(device_seed)
            .sign(&preimage)
            .to_bytes();

        let device = DeviceSignedOp {
            device_key_id: device_key_id.to_owned(),
            op: DeviceOp::PublishDirect,
            signature,
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
    /// `current` (so a child right after the genesis seed bases on `(1,1)`), the server-trusted child ids are
    /// recomputed over `(parents = [parent], tree, author, message)`, the device op is signed over them, and
    /// the publish runs through the same CAS/availability/lineage/sign/receipt backbone. Test-only.
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
        device_seed: &[u8; 32],
        op_id: &OpId,
        parent: CommitId,
        files: Vec<crate::UploadedFile>,
        author: &str,
        message: &str,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        use ed25519_dalek::{Signer as _, SigningKey};
        use topos_core::digest::{self, ManifestEntry};
        use topos_core::sign::{self, Commit, DeviceOp, DeviceOpFields, device_op_preimage};

        // The base generation a child must match is whatever `current` is right now (after a genesis seed,
        // `(1,1)`) — read it from the live signed record so the CAS in `publish` accepts the move.
        let record_bytes = self.read_signed_record(ws, skill).await?.ok_or_else(|| {
            AuthorityError::RejectedUpload("no current to base a child on".to_owned())
        })?;
        let record: topos_types::SignedCurrentRecord =
            serde_json::from_slice(&record_bytes).map_err(AuthorityError::internal)?;
        let expected = record.record.generation;

        // The server-trusted child ids — identical to what `publish`'s ingest recomputes, with the single
        // trunk parent — so the device op signs exactly what the in-transaction authz reconstructs.
        let manifest: Vec<ManifestEntry> = files
            .iter()
            .map(|f| ManifestEntry {
                path: f.path.clone(),
                mode: f.mode,
                content_sha256: digest::sha256(&f.bytes),
            })
            .collect();
        let bundle_digest = digest::bundle_digest(&manifest)
            .map_err(|r| AuthorityError::RejectedUpload(format!("{r:?}")))?;
        let version_id = sign::commit_id(&Commit {
            parents: &[parent.0],
            tree: bundle_digest,
            author,
            message,
        })
        .map_err(|e| AuthorityError::RejectedUpload(format!("{e:?}")))?;

        let op_id_bytes = uuid::Uuid::parse_str(op_id.as_str())
            .map_err(|_| {
                AuthorityError::RejectedUpload("op_id is not a canonical UUID".to_owned())
            })?
            .into_bytes();
        let fields = DeviceOpFields {
            workspace_id: ws.as_str(),
            skill_id: skill.as_str(),
            op: DeviceOp::PublishDirect,
            op_id: op_id_bytes,
            device_key_id,
            expected_epoch: expected.epoch,
            expected_seq: expected.seq,
            commit_id: version_id,
            bundle_digest,
        };
        let preimage = device_op_preimage(&fields)
            .map_err(|e| AuthorityError::RejectedUpload(format!("{e:?}")))?;
        let signature = SigningKey::from_bytes(device_seed)
            .sign(&preimage)
            .to_bytes();

        let device = DeviceSignedOp {
            device_key_id: device_key_id.to_owned(),
            op: DeviceOp::PublishDirect,
            signature,
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

    /// Corrupt the skill's stored signed `current` record so its signature no longer verifies, leaving the
    /// `(epoch, seq)` generation AND the named `version_id` UNCHANGED. Reads the live record, flips ONE byte
    /// of its base64url signature value to a different (still well-formed) character, and writes it back via
    /// [`force_signed_record`](crate::db::Db::force_signed_record). A follower then fetches a record whose
    /// version/generation look advanced but whose signature fails the pinned-key check → a refuse/ALARM that
    /// retains last-known-good. Test-only.
    ///
    /// # Errors
    /// [`AuthorityError::RejectedUpload`] if the skill has no signed `current` yet;
    /// [`AuthorityError::Internal`] on a (de)serialization or database fault.
    pub async fn tamper_current_signature(&self, ws: &WorkspaceId, skill: &SkillId) -> Result<()> {
        let record_bytes = self.read_signed_record(ws, skill).await?.ok_or_else(|| {
            AuthorityError::RejectedUpload("no signed current to tamper".to_owned())
        })?;
        let mut record: topos_types::SignedCurrentRecord =
            serde_json::from_slice(&record_bytes).map_err(AuthorityError::internal)?;
        // Flip exactly the first character of the base64url-unpadded signature (all ASCII, so byte 0 IS a
        // whole char) to a guaranteed-different valid one — the record still parses, but the 64-byte
        // signature it decodes to no longer matches, so `verify_pointer` fails.
        let first =
            record.signature.value.chars().next().ok_or_else(|| {
                AuthorityError::RejectedUpload("empty signature value".to_owned())
            })?;
        let replacement = if first == 'A' { "B" } else { "A" };
        record.signature.value.replace_range(0..1, replacement);
        let new_bytes = serde_json::to_vec(&record).map_err(AuthorityError::internal)?;
        self.db().force_signed_record(ws, skill, &new_bytes).await
    }
}
