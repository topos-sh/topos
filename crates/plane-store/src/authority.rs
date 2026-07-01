//! The sealed authority facade — the crate's one public type.

use std::path::{Path, PathBuf};

use topos_gitstore::{LocalLargeStore, Store};

use crate::db::Db;
use crate::enroll::{
    ConfirmOutcome, CreateInviteOutcome, DeviceAuthPoll, DeviceAuthStart, EnrollmentConfig,
    EnrollmentState, GovernanceOp, GovernanceOutcome, GovernanceSignedOp, InviteBootstrap,
    NoEnrollmentConfig, PasscodeComplete, PasscodeStart, RedeemOutcome, VerificationContext,
};
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, OpId, Principal, SkillId, WorkspaceId};
use crate::lineage::{CandidateCommit, LineageDecision};
use crate::read::{CurrentPointer, OpenProposalSummary, ReadScope, VersionMeta};
use crate::set_current::{DeviceSignedOp, SetCurrentReceipt};
use crate::signer::PlaneSigner;
use crate::upload::CandidateUpload;

/// The default size at/above which a file blob is offloaded to the large-object store (≈ 1 MiB). Git
/// packs/dedups small text-shaped blobs well but degrades on large binaries; below this they stay in git.
pub(crate) const DEFAULT_LARGE_THRESHOLD: u64 = 1 << 20;

/// The default per-blob hard reject cap (≈ 100 MiB): a blob larger than this is refused at ingest before
/// any bytes are staged.
pub(crate) const DEFAULT_LARGE_REJECT_CAP: u64 = 100 << 20;

/// The plane's per-workspace storage authority — the **only** public type in this crate.
///
/// Every raw SQL statement and raw git-object read is private; the only operations are authorized: the
/// skill-scoped [`read_object`](Self::read_object); the pointer-move writes
/// [`publish`](Self::publish) / [`revert`](Self::revert); the contribute writes [`propose`](Self::propose) /
/// [`review_approve`](Self::review_approve) / [`review_reject`](Self::review_reject); and the read-only
/// [`check_lineage`](Self::check_lineage). It owns one SQLite database (the per-workspace provenance,
/// reachability, roster, and pointer rows, every one bound on `workspace_id`) and a confined root under
/// which each workspace gets its own git object store. Cross-workspace isolation is that database
/// binding — never the directory.
#[derive(Debug)]
pub struct Authority {
    db: Db,
    git_root: PathBuf,
    /// The confined root under which each workspace gets its **own** large-object store (a sibling of
    /// `git_root`); big blobs are offloaded here at migrate. Per-workspace subdirs are the hard tenant
    /// boundary (no cross-workspace dedup), exactly like `git_root`.
    large_root: PathBuf,
    /// Size at/above which a file blob is offloaded to the large-object store (operational config; never
    /// enters any id/digest).
    large_threshold: u64,
    /// Per-blob hard reject cap, enforced at ingest.
    large_reject_cap: u64,
    /// The in-process plane signer — the ONLY private-key holder, loaded by
    /// [`with_plane_key`](Self::with_plane_key). Absent until configured: the pointer-move requires it (a
    /// typed precondition), while every other operation (read/upload/lineage/lifecycle) never signs.
    signer: Option<PlaneSigner>,
    /// The enrollment + governance issuance state — the `0600` HMAC secret + the static config, loaded by
    /// [`with_enrollment_config`](Self::with_enrollment_config). Absent until configured: every
    /// enrollment/governance op requires it (a typed precondition); every other op never touches it.
    enrollment: Option<EnrollmentState>,
}

impl Authority {
    /// Open the authority over a Postgres `database_url`, a git-store root, and a large-object-store root
    /// (the roots created if absent; the schema migrated on the database). The size-routing threshold +
    /// reject cap default to the crate's `DEFAULT_LARGE_THRESHOLD` / `DEFAULT_LARGE_REJECT_CAP`; override
    /// with [`with_large_limits`](Self::with_large_limits).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if a store root cannot be created or the database cannot be opened or
    /// migrated.
    pub async fn open(database_url: &str, git_root: &Path, large_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(git_root).map_err(AuthorityError::internal)?;
        std::fs::create_dir_all(large_root).map_err(AuthorityError::internal)?;
        let db = Db::connect(database_url).await?;
        Ok(Self {
            db,
            git_root: git_root.to_path_buf(),
            large_root: large_root.to_path_buf(),
            large_threshold: DEFAULT_LARGE_THRESHOLD,
            large_reject_cap: DEFAULT_LARGE_REJECT_CAP,
            signer: None,
            enrollment: None,
        })
    }

    /// Build the authority over an **already-open** `PgPool` (the schema assumed already migrated) plus the
    /// two store roots. The injection seam for `#[sqlx::test]` — which provisions a fresh per-test database,
    /// runs the migrations, and hands over the pool — and for an out-of-crate e2e harness that provisions its
    /// own per-test database the same way. Test / `test-fixtures` only: it is the sole place a `sqlx` type
    /// (`PgPool`) crosses this boundary, and it is compiled out of the production build.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if a store root cannot be created.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn from_pool(pool: sqlx::PgPool, git_root: &Path, large_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(git_root).map_err(AuthorityError::internal)?;
        std::fs::create_dir_all(large_root).map_err(AuthorityError::internal)?;
        Ok(Self {
            db: Db::from_pool(pool),
            git_root: git_root.to_path_buf(),
            large_root: large_root.to_path_buf(),
            large_threshold: DEFAULT_LARGE_THRESHOLD,
            large_reject_cap: DEFAULT_LARGE_REJECT_CAP,
            signer: None,
            enrollment: None,
        })
    }

    /// Load (or first-run generate + persist `0600`) the enrollment HMAC secret from the config's
    /// `secret_path`, enabling the enrollment + governance ops. Mirrors [`with_plane_key`](Self::with_plane_key)
    /// (the secret's custody is the plane key's exact custody — re-validated owner-only, atomically published)
    /// and holds the static config the bootstrap reads. The secret is read once here — never per-op, never
    /// inside a transaction.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if the secret file cannot be read/created/validated.
    pub fn with_enrollment_config(mut self, config: EnrollmentConfig) -> Result<Self> {
        self.enrollment = Some(EnrollmentState::load(config)?);
        Ok(self)
    }

    /// Load (or, on first run, generate + persist `0600`) the plane signing key from `path`, enabling the
    /// pointer-move. The key is read once here — never per-op, never inside a transaction. Self-host needs
    /// zero config (the key is generated on first run); an operator may pre-place a 32-byte seed at `path`
    /// instead. At-rest encryption / KMS is the named next step.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if the key file cannot be read/created/validated.
    pub fn with_plane_key(mut self, path: &Path) -> Result<Self> {
        self.signer = Some(PlaneSigner::load_or_generate(path)?);
        Ok(self)
    }

    /// Override the size-routing threshold + per-blob reject cap (operational config — neither ever enters
    /// a manifest, digest, or id). A consuming server wires these from its config; tests use it to force a
    /// placement (a tiny threshold routes ordinary bytes to the large store, proving identity is the same
    /// whichever store holds them).
    #[must_use]
    pub fn with_large_limits(mut self, threshold: u64, reject_cap: u64) -> Self {
        self.large_threshold = threshold;
        self.large_reject_cap = reject_cap;
        self
    }

    /// The size at/above which a file blob is offloaded to the large-object store.
    pub(crate) fn large_threshold(&self) -> u64 {
        self.large_threshold
    }

    /// The per-blob hard reject cap enforced at ingest.
    pub(crate) fn large_reject_cap(&self) -> u64 {
        self.large_reject_cap
    }

    /// Read one object's bytes through the skill-scoped access rule.
    ///
    /// The bytes are returned only if `principal` is rostered for `skill` **and** some commit of that
    /// skill reaches `object_id`. Every not-entitled and not-found case — not rostered, the skill does
    /// not reach the object, or the object does not exist — returns the single [`AuthorityError::NotFound`],
    /// byte-for-byte indistinguishable, so a caller can never probe which skills or objects exist.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] when not entitled / not reachable / nonexistent;
    /// [`AuthorityError::Integrity`] if the authority's provenance and its object store have diverged
    /// (corruption — reachable only *after* entitlement was proven, so it discloses nothing);
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn read_object(
        &self,
        principal: &Principal,
        ws: &WorkspaceId,
        skill: &SkillId,
        object_id: ObjectId,
    ) -> Result<Vec<u8>> {
        crate::read::read_object(self, principal, ws, skill, object_id).await
    }

    /// Resolve a presented **read token** to its opaque [`ReadScope`] — the read-credential resolver (the
    /// entry point for every authenticated read). Only the token's sha256 is stored, so the plaintext is
    /// never recoverable from the database; an unknown token is the single indistinguishable
    /// [`AuthorityError::NotFound`]. The returned scope is a capability: it is passed back to
    /// [`serve_object`](Self::serve_object) / [`read_current`](Self::read_current) /
    /// [`read_version_metadata`](Self::read_version_metadata), never parsed by the caller.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on an unknown token; [`AuthorityError::Internal`] on a database fault;
    /// [`AuthorityError::Integrity`] if a stored token row is corrupt.
    pub async fn resolve_read_token(&self, token: &str, now: i64) -> Result<ReadScope> {
        crate::read::resolve_read_token(self, token, now).await
    }

    /// Read a skill's signed `current` pointer for an authenticated [`ReadScope`] — the public authenticated
    /// pointer-fetch surface (what a follower's currency check returns). `None` until the pointer has been
    /// moved (signed); otherwise the raw `SignedCurrentRecord` bytes plus the extracted `(epoch, seq)`.
    ///
    /// # Errors
    /// [`AuthorityError::Integrity`] if the stored record blob is unparseable (corruption, never not-found);
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn read_current(&self, scope: &ReadScope) -> Result<Option<CurrentPointer>> {
        crate::read::read_current(self, scope).await
    }

    /// Serve one object's bytes for an authenticated [`ReadScope`], asserting the scope's `(ws, skill)`
    /// matches the request path's. A scope/path mismatch or a malformed object id is the single
    /// indistinguishable [`AuthorityError::NotFound`]; otherwise the read goes through the skill-scoped
    /// [`read_object`](Self::read_object).
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on a scope/path mismatch, a malformed id, or an unreachable object;
    /// [`AuthorityError::Integrity`]/[`AuthorityError::Internal`] as [`read_object`](Self::read_object).
    pub async fn serve_object(
        &self,
        scope: &ReadScope,
        req_ws: &str,
        req_skill: &str,
        object_id_hex: &str,
    ) -> Result<Vec<u8>> {
        crate::read::serve_object(self, scope, req_ws, req_skill, object_id_hex).await
    }

    /// Read a version's authenticated metadata for a [`ReadScope`] (the version-metadata route's core):
    /// `(version_id, parents, author, message, bundle_digest, files)`, assembled WITHOUT reading any blob
    /// bytes. Asserts the scope/path match, R1-authorizes the version read (rostered ∧ accepted-trunk or
    /// open-non-stale proposal), and returns the single indistinguishable [`AuthorityError::NotFound`] for an
    /// unauthorized/unreachable version (never a `403`).
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on scope/path mismatch, a bad id, or an unauthorized/unreachable version;
    /// [`AuthorityError::Integrity`] on a provenance/store divergence; [`AuthorityError::Internal`] on a
    /// database fault.
    pub async fn read_version_metadata(
        &self,
        scope: &ReadScope,
        req_ws: &str,
        req_skill: &str,
        version_id_hex: &str,
    ) -> Result<VersionMeta> {
        crate::read::read_version_metadata(self, scope, req_ws, req_skill, version_id_hex).await
    }

    /// List a skill's OPEN, non-stale proposals for an authenticated [`ReadScope`] (the proposals-listing
    /// route's core): each `(version_id, base, created_at)` — **NO bytes, NO proposer, NO roles, NO rendered
    /// view**. Asserts the scope/path match (the cross-skill/workspace leak guard) and enumerates the
    /// rostered ∧ `open ∧ base == current` rows, so a staled proposal vanishes exactly as it drops out of the
    /// object/version reads (keep == read == list). A NON-rostered principal with a valid token yields an
    /// EMPTY list, never a not-found (the roster JOIN is the authz; there is no per-row probe); a scope/path
    /// mismatch is the single indistinguishable [`AuthorityError::NotFound`]. This is a READ — nothing is
    /// signed, no governance is consulted, no body is taken.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on a scope/path mismatch; [`AuthorityError::Integrity`] if a stored row is
    /// corrupt; [`AuthorityError::Internal`] on a database fault.
    pub async fn list_open_proposals(
        &self,
        scope: &ReadScope,
        req_ws: &str,
        req_skill: &str,
    ) -> Result<Vec<OpenProposalSummary>> {
        crate::read::list_open_proposals(self, scope, req_ws, req_skill).await
    }

    /// Evaluate the cross-skill lineage predicate over a candidate set (a read-only gather + the pure
    /// decision; the pointer-move write enforces the same rule transactionally at promote/propose time).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn check_lineage(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        candidates: &[CandidateCommit],
    ) -> Result<LineageDecision> {
        crate::lineage::check_lineage(self, ws, skill, candidates).await
    }

    /// Publish a candidate as the skill's new `current` — a direct publish, or **genesis** for the first
    /// version. The full backbone flow: a review-required preflight (uploads nothing if gated) → ingest +
    /// migrate (the crash-safe quarantine → lease → install → record) → the one pure-DB pointer-move
    /// transaction (compare-and-set, sign, re-root, durable receipt). Returns the durable, replayable
    /// receipt; a retry with the same `op_id` + bound identity returns it byte-identically.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`]/[`AuthorityError::Integrity`] on a store fault; the plane key must be
    /// configured ([`with_plane_key`](Self::with_plane_key)).
    #[allow(clippy::too_many_arguments)]
    pub async fn publish(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        op_id: &OpId,
        candidate: CandidateUpload,
        device: DeviceSignedOp,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        // A direct publish must be signed as exactly that. Forwarding an arbitrary device op (e.g. a
        // `Revert`-labelled candidate of new bytes) would skip the direct-publish review gate while still
        // reaching the promote path — a review bypass. Reject anything but `PublishDirect` BEFORE ingesting
        // (so a misuse uploads/migrates/leases nothing).
        if !matches!(device.op, topos_core::sign::DeviceOp::PublishDirect) {
            return crate::set_current::reject_non_publish_op(
                self, ws, skill, op_id, &device, created_at,
            )
            .await;
        }
        if let Some(receipt) = crate::set_current::publish_preflight(
            self,
            ws,
            skill,
            device.op,
            &device.device_key_id,
            op_id,
            None,
            None,
            device.expected,
            created_at,
        )
        .await?
        {
            return Ok(receipt);
        }
        let staged = crate::lifecycle::ingest(self, ws, op_id, candidate, now).await?;
        crate::lifecycle::migrate(self, ws, &staged, now).await?;
        crate::set_current::publish(self, ws, skill, &staged, &device, created_at, now).await
    }

    /// Revert the skill's `current` to a known-good prior version — a **forward** commit `{tree: good.tree,
    /// parents: [current]}` that advances `seq` (the pointer never moves backward). Bypasses the review gate
    /// (it restores already-consented bytes — the in-v0 safety net).
    ///
    /// # Errors
    /// As [`publish`](Self::publish); plus a git-store fault constructing the forward commit.
    #[allow(clippy::too_many_arguments)]
    pub async fn revert(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        good: CommitId,
        device: DeviceSignedOp,
        author: &str,
        message: &str,
        op_id: &OpId,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        // A revert must be signed as exactly `Revert` (mirroring the publish/propose/review guards): a
        // mismatched op would otherwise mis-route into the promote arms. Reject it before constructing the
        // forward commit, recording nothing.
        if !matches!(device.op, topos_core::sign::DeviceOp::Revert) {
            return crate::set_current::reject_op_mismatch(
                self,
                ws,
                skill,
                op_id,
                &device,
                created_at,
                "a revert must be signed as Revert",
            )
            .await;
        }
        crate::set_current::revert(
            self, ws, skill, good, &device, author, message, op_id, created_at, now,
        )
        .await
    }

    /// Open a **proposal** — upload a candidate version for review WITHOUT moving `current` (the contribute
    /// motion's first half). The full flow: ingest + migrate (the crash-safe quarantine → lease → install →
    /// record, exactly as `publish`), then the one pure-DB transaction opens a `proposals` row and roots the
    /// candidate's bytes through `proposal_object` (gated on `open ∧ non-stale` for both retention and read).
    /// Returns `NEEDS_REVIEW`; `current` is byte-for-byte unchanged and nothing is signed. A later
    /// [`review_approve`](Self::review_approve) promotes it. Genesis cannot be proposed (publish the first
    /// version directly); a `--propose` against a skill with no `current` is a typed failure that uploads nothing.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`]/[`AuthorityError::Integrity`] on a store fault; the plane key must be
    /// configured ([`with_plane_key`](Self::with_plane_key)).
    #[allow(clippy::too_many_arguments)]
    pub async fn propose(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        op_id: &OpId,
        candidate: CandidateUpload,
        device: DeviceSignedOp,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        // A proposal must be signed as exactly `PublishPropose`. Forwarding another device op could reach the
        // promote path (which moves `current`) — a gate bypass — so reject anything else BEFORE ingesting (a
        // misuse uploads/migrates/opens nothing).
        if !matches!(device.op, topos_core::sign::DeviceOp::PublishPropose) {
            return crate::set_current::reject_op_mismatch(
                self,
                ws,
                skill,
                op_id,
                &device,
                created_at,
                "a proposal must be signed as PublishPropose",
            )
            .await;
        }
        // Genesis cannot be proposed (a proposal needs an existing base). A cheap pre-ingest check; the
        // in-transaction None branch is the authoritative backstop.
        if self.db().read_current_commit(ws, skill).await?.is_none() {
            return crate::set_current::reject_op_mismatch(
                self,
                ws,
                skill,
                op_id,
                &device,
                created_at,
                "cannot propose against a skill with no current version; publish the genesis version directly",
            )
            .await;
        }
        let staged = crate::lifecycle::ingest(self, ws, op_id, candidate, now).await?;
        crate::lifecycle::migrate(self, ws, &staged, now).await?;
        crate::set_current::propose(self, ws, skill, &staged, &device, created_at, now).await
    }

    /// **Approve** an open proposal — promote it to `current` (the sideways move; the contribute motion's
    /// second half). Uploads/leases/migrates nothing (the candidate is already in the main store, rooted by
    /// its proposal); runs only the one pointer-move transaction, which compare-and-sets on the proposal's
    /// base, performs the `proposal_object → commit_object` handoff, signs the advanced pointer, and flips the
    /// proposal to `accepted`. A stale base ⇒ `CONFLICT` (rebase + re-propose); approving an already-resolved
    /// proposal ⇒ a typed `CONFLICT`/`DENIED`, never a second promote. Under `review_required`, an approve
    /// whose principal is the proposer's is rejected (four-eyes).
    ///
    /// # Errors
    /// As [`publish`](Self::publish); plus a genuine integrity fault if a non-stale proposal's bytes are lost.
    #[allow(clippy::too_many_arguments)]
    pub async fn review_approve(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        commit: CommitId,
        device: DeviceSignedOp,
        op_id: &OpId,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        if !matches!(device.op, topos_core::sign::DeviceOp::ReviewApprove) {
            return crate::set_current::reject_op_mismatch(
                self,
                ws,
                skill,
                op_id,
                &device,
                created_at,
                "a review approval must be signed as ReviewApprove",
            )
            .await;
        }
        crate::set_current::review_approve(self, ws, skill, commit, &device, op_id, created_at, now)
            .await
    }

    /// **Reject** (or proposer-**withdraw**) an open proposal — flip it to `rejected`, moving no pointer and
    /// signing nothing, after which the gated root stops matching and ordinary GC reclaims its unique bytes.
    /// One path serves reviewer-reject and proposer-withdraw (`resolved_by` records who); rejecting an
    /// already-rejected proposal is an idempotent no-op, an accepted one a typed failure.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a store fault.
    #[allow(clippy::too_many_arguments)]
    pub async fn review_reject(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        commit: CommitId,
        device: DeviceSignedOp,
        op_id: &OpId,
        created_at: &str,
    ) -> Result<SetCurrentReceipt> {
        if !matches!(device.op, topos_core::sign::DeviceOp::ReviewReject) {
            return crate::set_current::reject_op_mismatch(
                self,
                ws,
                skill,
                op_id,
                &device,
                created_at,
                "a review rejection must be signed as ReviewReject",
            )
            .await;
        }
        crate::set_current::review_reject(self, ws, skill, commit, &device, op_id, created_at).await
    }

    // ── enrollment + governance issuance (every op decided in-Authority; requires with_enrollment_config) ──

    /// Create an owner-signed **invite** — mint the opaque `/i/<token>` link, store it (hash-only), seed the
    /// invited members at `role` (`status = 'invited'`), and record the governance receipt. GOVERNANCE-signed
    /// (the signing owner's device-op signature is verified in-transaction). `op_id`-idempotent: a retry with
    /// the matching bound identity re-derives the IDENTICAL link and replays the receipt; a different request
    /// under the same op id is a denied key-reuse. The role + skills come from `signed.op` (a `GovernanceOp::Invite`).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config / plane key is set; a database fault.
    pub async fn create_invite(
        &self,
        ws: &WorkspaceId,
        op_id: &str,
        signed: GovernanceSignedOp,
        created_at: &str,
    ) -> Result<CreateInviteOutcome> {
        crate::enroll::create_invite(self, ws, op_id, &signed, created_at).await
    }

    /// Resolve an invite link to its **bootstrap payload** — the workspace identity, the offered skills, and
    /// the plane signing root to TOFU-pin (no bytes, no role). A revoked/expired/absent invite is the single
    /// indistinguishable [`AuthorityError::NotFound`].
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on a dead/unknown invite; [`AuthorityError::Internal`] on a fault.
    pub async fn read_invite_bootstrap(&self, token: &str, now: i64) -> Result<InviteBootstrap> {
        crate::enroll::read_invite_bootstrap(self, token, now).await
    }

    /// Resolve a device-auth `user_code` to its **verification-page disclosure** — the machine name + device
    /// fingerprint, the workspace identity, and the offered skills a human reviews before confirming (the
    /// RFC-8628 confused-deputy guard). Carries no secret. A miss / non-live / expired session — or an unknown
    /// code — is the single indistinguishable [`AuthorityError::NotFound`].
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on no live session; [`AuthorityError::Internal`] on a database fault.
    pub async fn read_verification_context(
        &self,
        user_code: &str,
        now: i64,
    ) -> Result<VerificationContext> {
        crate::enroll::read_verification_context(self, user_code, now).await
    }

    /// Confirm a session's identity from an **externally-proven** email (the OIDC callback's in-Authority
    /// half) — set the live session's `confirmed_principal` + status `confirmed`, so the device's next poll
    /// yields a grant. The CALLER (the OIDC module) MUST have validated the id_token first; this op trusts the
    /// passed `verified_email` and parses it INSIDE the op (never a handler `Principal::parse`). An unknown /
    /// non-live session — or a malformed email — is the indistinguishable [`AuthorityError::NotFound`].
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on no live session / a malformed email; [`AuthorityError::Internal`] on a fault.
    pub async fn confirm_external_identity(
        &self,
        user_code: &str,
        verified_email: &str,
        now: i64,
    ) -> Result<ConfirmOutcome> {
        crate::enroll::confirm_external_identity(self, user_code, verified_email, now).await
    }

    /// Start a **device-authorization** flow against an invite (RFC-8628-shaped). The device key id is
    /// SERVER-derived from `device_public_key` (a client-asserted id is ignored). Returns the secret device
    /// code, the user code, and the verification URI. Cloud sessions await a human identity step; self-host
    /// sessions are born confirmed (a device-rooted principal), so the first poll yields a grant.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on a dead/unknown invite; [`AuthorityError::Internal`] on a fault.
    pub async fn start_device_auth(
        &self,
        invite_token: &str,
        device_public_key: &[u8; 32],
        machine_name: &str,
        now: i64,
        created_at: &str,
    ) -> Result<DeviceAuthStart> {
        crate::enroll::start_device_auth(
            self,
            invite_token,
            device_public_key,
            machine_name,
            now,
            created_at,
        )
        .await
    }

    /// Poll a device-authorization session — `Pending`/`SlowDown`/`Denied`/`Expired`, or `Granted` with the
    /// single-use enrollment grant (a re-poll re-derives the SAME grant). An unknown device code is the
    /// indistinguishable [`AuthorityError::NotFound`].
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on an unknown device code; [`AuthorityError::Internal`] on a fault.
    pub async fn poll_device_auth(
        &self,
        device_code: &str,
        now: i64,
        created_at: &str,
    ) -> Result<DeviceAuthPoll> {
        crate::enroll::poll_device_auth(self, device_code, now, created_at).await
    }

    /// Start a **passcode** challenge for an email on a live session — store the 6-digit code (hash-only) and
    /// return the plaintext ONCE (for the mailer; never logged). A constant-shaped ack (no roster-enumeration
    /// oracle — the cloud gate is enforced at redeem). The email is parsed INSIDE the op.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on no live session (or a malformed email); [`AuthorityError::Internal`] on a fault.
    pub async fn start_passcode(
        &self,
        user_code: &str,
        email: &str,
        now: i64,
        created_at: &str,
    ) -> Result<PasscodeStart> {
        crate::enroll::start_passcode(self, user_code, email, now, created_at).await
    }

    /// Complete a passcode challenge — verify the code under the TTL + attempt cap, confirming the session's
    /// identity on success. The email is parsed INSIDE the op.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on no live session (or a malformed email); [`AuthorityError::Internal`] on a fault.
    pub async fn complete_passcode(
        &self,
        user_code: &str,
        email: &str,
        code: &str,
        now: i64,
    ) -> Result<PasscodeComplete> {
        crate::enroll::complete_passcode(self, user_code, email, code, now).await
    }

    /// **Redeem** an enrollment grant — THE central op. In one transaction: re-derive the device key id, check
    /// the grant binds this device, verify the enrollment possession proof against the grant's bound key
    /// ([`topos_core::sign::verify_enroll`]), apply the roster gate (cloud requires a confirmed identity;
    /// self-host grants membership), register the device, and mint per-skill read tokens. **Returns no user
    /// token, ever.** Naturally idempotent — a replay re-derives identical read tokens.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    pub async fn redeem_enrollment(
        &self,
        grant_token: &str,
        enroll_sig: &[u8; 64],
        device_public_key: [u8; 32],
        now: i64,
        created_at: &str,
    ) -> Result<RedeemOutcome> {
        crate::enroll::redeem_enrollment(
            self,
            grant_token,
            enroll_sig,
            device_public_key,
            now,
            created_at,
        )
        .await
    }

    /// **Admin claim** (self-host first-boot standup) — consume the one-time claim token, create the
    /// (self-host) workspace, seat its first owner (a server-derived device-rooted principal), and register
    /// the claiming device. Returns an [`EnrollmentRedeemed`](crate::EnrollmentRedeemed)-shaped result.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    pub async fn admin_claim(
        &self,
        claim_token: &str,
        device_public_key: [u8; 32],
        display_name: &str,
        now: i64,
        created_at: &str,
    ) -> Result<RedeemOutcome> {
        crate::enroll::admin_claim(
            self,
            claim_token,
            device_public_key,
            display_name,
            now,
            created_at,
        )
        .await
    }

    /// **Set** a principal's workspace role (owner-only governance op, with the last-owner-lockout guard).
    /// GOVERNANCE-signed + `op_id`-idempotent. The role + target come from `signed.op` (a `GovernanceOp::RosterSet`).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    pub async fn roster_set(
        &self,
        ws: &WorkspaceId,
        op_id: &str,
        signed: GovernanceSignedOp,
        created_at: &str,
    ) -> Result<GovernanceOutcome> {
        if !matches!(signed.op, GovernanceOp::RosterSet { .. }) {
            return Ok(GovernanceOutcome::Denied("op is not a roster_set"));
        }
        crate::enroll::governance_mutation(self, ws, op_id, &signed, created_at).await
    }

    /// **Remove** a principal from the workspace roster (owner-only, with the last-owner-lockout guard).
    /// GOVERNANCE-signed + `op_id`-idempotent. The target comes from `signed.op` (a `GovernanceOp::RosterRemove`).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    pub async fn roster_remove(
        &self,
        ws: &WorkspaceId,
        op_id: &str,
        signed: GovernanceSignedOp,
        created_at: &str,
    ) -> Result<GovernanceOutcome> {
        if !matches!(signed.op, GovernanceOp::RosterRemove { .. }) {
            return Ok(GovernanceOutcome::Denied("op is not a roster_remove"));
        }
        crate::enroll::governance_mutation(self, ws, op_id, &signed, created_at).await
    }

    /// **Revoke** a registered device — flip `revoked` AND drop its read tokens in one transaction (instant
    /// per-device revoke). Owner OR the device's own principal may sign. GOVERNANCE-signed + `op_id`-idempotent.
    /// The target device key id comes from `signed.op` (a `GovernanceOp::DeviceRevoke`).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    pub async fn revoke_device(
        &self,
        ws: &WorkspaceId,
        op_id: &str,
        signed: GovernanceSignedOp,
        created_at: &str,
    ) -> Result<GovernanceOutcome> {
        if !matches!(signed.op, GovernanceOp::DeviceRevoke { .. }) {
            return Ok(GovernanceOutcome::Denied("op is not a device_revoke"));
        }
        crate::enroll::governance_mutation(self, ws, op_id, &signed, created_at).await
    }

    /// Set the workspace's `review_required` policy — the off-by-default anti-poisoning gate. With it on, a
    /// direct publish short-circuits to `APPROVAL_REQUIRED` (ingesting nothing) and an approval requires a
    /// second, distinct reviewer (four-eyes); genesis + revert bypass it. This is the authorized public op a
    /// downstream plane (or its admin console) toggles; the test-only `seed_review_required` shim delegates
    /// to it. A trusted caller (the toggle is not itself device-op-signed — the device-signed governance
    /// route over this policy is later work); authorization to call it is the composing plane's concern.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn set_review_required(&self, ws: &WorkspaceId, review_required: bool) -> Result<()> {
        self.db.set_review_required(ws, review_required).await
    }

    /// The plane's raw 32-byte Ed25519 **public** key — for a follower to pin the trust root out-of-band.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no plane key is configured.
    pub fn plane_public_key(&self) -> Result<[u8; 32]> {
        Ok(self.plane_signer()?.public_key())
    }

    /// The plane's signing key id (the `key_id` in a signed pointer + an OK receipt).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no plane key is configured.
    pub fn plane_key_id(&self) -> Result<String> {
        Ok(self.plane_signer()?.key_id().to_owned())
    }

    /// Read back a skill's signed `current` record — the serialized `SignedCurrentRecord` bytes. `None`
    /// until the pointer has been moved (signed). **Unauthenticated** and `pub(crate)`: the public
    /// authenticated pointer-fetch surface is [`read_current`](Self::read_current), which takes a resolved
    /// [`ReadScope`]; this raw read is an internal building block (and the in-crate tests' assertion hook).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault.
    pub(crate) async fn read_signed_record(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
    ) -> Result<Option<Vec<u8>>> {
        self.db.read_signed_record(ws, skill).await
    }

    // ── pub(crate) internals the port modules drive ──────────────────────────────────────────────

    /// The SQLite backend handle (raw SQL stays inside `mod db`).
    pub(crate) fn db(&self) -> &Db {
        &self.db
    }

    /// The configured plane signer, or a typed precondition fault — the pointer-move requires a key.
    pub(crate) fn plane_signer(&self) -> Result<&PlaneSigner> {
        self.signer
            .as_ref()
            .ok_or_else(|| AuthorityError::internal(NoPlaneKey))
    }

    /// The configured enrollment state (secret + config), or a typed precondition fault — every
    /// enrollment/governance op requires it.
    pub(crate) fn enrollment(&self) -> Result<&EnrollmentState> {
        self.enrollment
            .as_ref()
            .ok_or_else(|| AuthorityError::internal(NoEnrollmentConfig))
    }

    /// Derive a deterministic opaque credential under the enrollment secret (the one credential mint). A
    /// thin wrapper over [`crate::enroll::derive_token`] that supplies the configured secret.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set.
    pub(crate) fn derive_token(&self, domain: &[u8], parts: &[&[u8]]) -> Result<String> {
        let secret = self.enrollment()?.secret.as_bytes();
        Ok(crate::enroll::derive_token(secret, domain, parts))
    }

    /// The per-workspace git-store directory — one component under the confined root. `WorkspaceId` is
    /// a validated path-safe id (no separators, no `..`), so this can never escape `git_root`.
    fn workspace_git_dir(&self, ws: &WorkspaceId) -> PathBuf {
        self.git_root.join(ws.as_str())
    }

    /// The per-op upload-quarantine directory: `git_root/<ws>.quarantine/<op_id>`. `WorkspaceId` forbids
    /// `.`, so `<ws>.quarantine` can never collide with a real workspace store dir (`git_root/<ws>`), and
    /// it is a SIBLING of that store — so the GC scanner, which walks only `git_root/<ws>/`, never sees a
    /// quarantine. Both ids are validated path-safe newtypes, so the path can never escape `git_root`.
    /// (Used by the not-yet-wired lifecycle ops, so unreferenced in a non-test production build.)
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn workspace_quarantine_dir(
        &self,
        ws: &WorkspaceId,
        op_id: &crate::id::OpId,
    ) -> PathBuf {
        self.git_root
            .join(format!("{}.quarantine", ws.as_str()))
            .join(op_id.as_str())
    }

    /// Open the per-workspace git store for reading. A failure here is reached only after the database
    /// authorized the read, so a missing/un-openable store is a provenance/store divergence (corruption).
    pub(crate) fn open_store(&self, ws: &WorkspaceId) -> Result<Store> {
        Store::open(&self.workspace_git_dir(ws)).map_err(AuthorityError::integrity)
    }

    /// Open-or-create the per-workspace git store for an upload's object write (the bare repo is created
    /// on a workspace's first upload).
    ///
    /// Open first, then create, then open again on a failed create: two concurrent first-time uploads to
    /// the same workspace can both observe the directory as absent, and bare-repo `init` is not an
    /// idempotent open-or-create — so the loser of the creation race falls back to opening what the winner
    /// just made instead of failing. (A finer-grained guard against a writer racing *mid*-init lands with
    /// the broader concurrency work.)
    pub(crate) fn store_for_write(&self, ws: &WorkspaceId) -> Result<Store> {
        let dir = self.workspace_git_dir(ws);
        match Store::open(&dir) {
            Ok(store) => Ok(store),
            Err(_) => match Store::init(&dir) {
                Ok(store) => Ok(store),
                Err(_) => Store::open(&dir).map_err(AuthorityError::internal),
            },
        }
    }

    /// The per-workspace large-object store handle, rooted at `large_root/<ws>/`. `WorkspaceId` is a
    /// validated, path-safe id (no separators, no `..`), so the root can never escape `large_root` and one
    /// workspace's handle can never name another's bytes — cross-workspace isolation is the path itself, and
    /// byte-identical content in two workspaces is two distinct physical objects (no cross-workspace dedup).
    /// Construction stays inside this crate, so nothing outside the authority can fetch a large object by
    /// bare hash. Infallible: the store creates its directories lazily on the first `put`.
    pub(crate) fn large_store(&self, ws: &WorkspaceId) -> LocalLargeStore {
        LocalLargeStore::new(self.large_root.join(ws.as_str()))
    }
}

// ── test-fixtures shims (feature-gated; NEVER part of the production API) ──────────────────────────────
//
// A clearly-marked, feature-gated surface a DOWNSTREAM test crate (the OSS plane's HTTP routes, the HERO
// loopback) drives to stage an authority without a real enrollment subsystem. Each shim only DRIVES an
// existing op or seed helper — it grants no capability the production API doesn't already enforce (a write
// still needs a registered, non-revoked, rostered device; a read still needs a minted token). Gated behind
// `feature = "test-fixtures"`, which the production `topos-plane` build never enables (a CI guard asserts
// it), so none of this ships.
#[cfg(feature = "test-fixtures")]
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
        self.db.seed_roster(ws, skill, principal).await
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
        self.db
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
        self.db.seed_read_token(ws, skill, principal, token).await
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
        self.db
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
        self.db
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
        self.publish(ws, skill, op_id, candidate, device, created_at, now)
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
        self.publish(ws, skill, op_id, candidate, device, created_at, now)
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

/// The pointer-move was attempted with no plane signing key configured (a precondition fault, not a
/// protocol outcome — wired as an internal error so no key state crosses the public boundary).
#[derive(Debug, thiserror::Error)]
#[error("no plane signing key configured (call with_plane_key)")]
struct NoPlaneKey;
