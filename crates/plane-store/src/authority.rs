//! The sealed authority facade — the crate's one public type.

use std::path::{Path, PathBuf};
use std::time::Duration;

use topos_gitstore::{LocalLargeStore, Store};

use crate::catalog::{LifecycleOutcome, PurgeOutcome};
use crate::channels::{
    ChannelMembershipOutcome, CurationOutcome, ProtectKind, ProtectLevel, ProtectOutcome,
    SubscriptionOutcome,
};
use crate::db::Db;
use crate::delivery::{AppliedSkill, Delivery};
use crate::enroll::DeploymentMode;
use crate::enroll::{
    ConfirmOutcome, DeviceAuthPoll, DeviceAuthStart, EnrollmentConfig, EnrollmentDisclosure,
    EnrollmentState, InviteBootstrap, NoEnrollmentConfig, PasscodeComplete, PasscodeStart,
    RedeemOutcome, VerificationContext,
};
use crate::error::{AuthorityError, Result};
use crate::governance::{
    ApproveStandupOutcome, CreateInviteOutcome, CreateWorkspaceOutcome, GovernanceOp,
    GovernanceOutcome, GovernanceRequest, MintClaimOutcome,
};
use crate::id::{CommitId, ObjectId, OpId, Principal, SkillId, WorkspaceId};
use crate::lineage::{CandidateCommit, LineageDecision};
use crate::read::{CurrentPointer, OpenProposalSummary, ReadScope, VersionMeta};
use crate::session_read::SkillIndexRow;
use crate::session_roster::{
    RosterView, SessionInviteOutcome, SessionInviteRole, SessionRotateOutcome,
};
use crate::set_current::{DeviceOpAuth, SetCurrentReceipt};
use crate::upload::CandidateUpload;

/// The default size at/above which a file blob is offloaded to the large-object store (≈ 1 MiB). Git
/// packs/dedups small text-shaped blobs well but degrades on large binaries; below this they stay in git.
pub(crate) const DEFAULT_LARGE_THRESHOLD: u64 = 1 << 20;

/// The default per-blob hard reject cap (≈ 100 MiB): a blob larger than this is refused at ingest before
/// any bytes are staged.
pub(crate) const DEFAULT_LARGE_REJECT_CAP: u64 = 100 << 20;

/// Connection-pool tuning for the Postgres backend — plain owned data (no `sqlx` type crosses it), so a
/// composing plane sets it without naming the driver. `None` on a field keeps the default: sqlx's
/// `max_connections = 10` / `acquire_timeout = 30s`, and the server's own statement/lock/idle values. The
/// OSS bin fills this from `TOPOS_PLANE_DB_*` inside `PlaneState::open` (the one place the env is read);
/// tests open with [`PoolConfig::default`] via [`Authority::open`]. Each `Some` timeout is applied as a
/// session `SET` on every pooled connection (see [`Authority::open_with_pool`]).
#[derive(Debug, Clone, Default)]
pub struct PoolConfig {
    /// Max pooled connections (sqlx's default 10 when `None`). Raise it for a plane serving concurrent HTTP:
    /// a write holds one connection for the whole `run_serializable!` retry loop, so 10 can bottleneck under
    /// contention or once one plane fronts many workspaces.
    pub max_connections: Option<u32>,
    /// How long `acquire` waits for a free pooled connection before failing (sqlx's default 30s when `None`).
    pub acquire_timeout: Option<Duration>,
    /// Per-statement server ceiling (`statement_timeout`). `None` ⇒ unset (the server default). Opt in for a
    /// hard runaway-query ceiling, but keep it above the slowest legitimate whole-bundle render.
    pub statement_timeout: Option<Duration>,
    /// Lock-wait ceiling (`lock_timeout`). `None` ⇒ unset (the server default).
    pub lock_timeout: Option<Duration>,
    /// How long a transaction may sit idle — open but running no statement — before the server aborts it
    /// (`idle_in_transaction_session_timeout`), bounding an abandoned/stuck txn that would otherwise pin row
    /// locks. `None` ⇒ unset; a modest value is safe here because every write txn is pure-DB and short.
    pub idle_in_transaction_timeout: Option<Duration>,
}

/// The plane's per-workspace storage authority — the **only** public type in this crate.
///
/// Every raw SQL statement and raw git-object read is private; the only operations are authorized: the
/// skill-scoped [`read_object`](Self::read_object); the pointer-move writes
/// [`publish`](Self::publish) / [`revert`](Self::revert); the contribute writes [`propose`](Self::propose) /
/// [`review_approve`](Self::review_approve) / [`review_reject`](Self::review_reject); and the read-only
/// [`check_lineage`](Self::check_lineage). It owns one Postgres database (the per-workspace provenance,
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
        Self::open_with_pool(database_url, git_root, large_root, PoolConfig::default()).await
    }

    /// Open the authority exactly like [`open`](Self::open) but with explicit connection-pool tuning
    /// ([`PoolConfig`]) — `open` is this with [`PoolConfig::default`]. The OSS bin uses this to apply the
    /// operator's `TOPOS_PLANE_DB_*` settings.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if a store root cannot be created or the database cannot be opened or
    /// migrated.
    pub async fn open_with_pool(
        database_url: &str,
        git_root: &Path,
        large_root: &Path,
        pool: PoolConfig,
    ) -> Result<Self> {
        std::fs::create_dir_all(git_root).map_err(AuthorityError::internal)?;
        std::fs::create_dir_all(large_root).map_err(AuthorityError::internal)?;
        let db = Db::connect(database_url, &pool).await?;
        Ok(Self {
            db,
            git_root: git_root.to_path_buf(),
            large_root: large_root.to_path_buf(),
            large_threshold: DEFAULT_LARGE_THRESHOLD,
            large_reject_cap: DEFAULT_LARGE_REJECT_CAP,
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
            enrollment: None,
        })
    }

    /// Load (or first-run generate + persist `0600`) the enrollment HMAC secret from the config's
    /// `secret_path`, enabling the enrollment + governance ops. Uses the same `0600` seed custody
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
    /// The bytes are returned only if `principal` is a confirmed member of `ws` **and** some commit of
    /// that skill reaches `object_id`. Every not-entitled and not-found case — not a member, the skill
    /// does not reach the object, or the object does not exist — returns the single
    /// [`AuthorityError::NotFound`],
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

    /// Resolve a presented **workspace credential** to an opaque [`ReadScope`] on one of the
    /// workspace's skills — the read-credential resolver (the entry point for every authenticated
    /// device read). The credential's sha256 selects its registry row bound to the CALLER'S CLAIMED
    /// workspace; the row's principal must be a CONFIRMED workspace member (the membership join IS
    /// the read authorization — deleting the membership row kills reads immediately). Only the sha256
    /// is stored, so the plaintext is never recoverable from the database; every miss is the single
    /// indistinguishable [`AuthorityError::NotFound`]. The returned scope is a capability: it is
    /// passed back to [`serve_object`](Self::serve_object) / [`read_current`](Self::read_current) /
    /// [`read_version_metadata`](Self::read_version_metadata), never parsed by the caller.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on any miss (unknown/revoked credential, non-member, malformed
    /// ids); [`AuthorityError::Internal`] on a database fault; [`AuthorityError::Integrity`] if a
    /// stored row is corrupt.
    pub async fn resolve_read_scope(
        &self,
        ws: &str,
        skill: &str,
        credential: &str,
    ) -> Result<ReadScope> {
        let ws = WorkspaceId::parse(ws).map_err(|_| AuthorityError::NotFound)?;
        let skill = SkillId::parse(skill).map_err(|_| AuthorityError::NotFound)?;
        crate::enroll::resolve_read_scope(self, ws, skill, credential).await
    }

    /// Read a skill's `current` pointer for an authenticated [`ReadScope`] — the public authenticated
    /// pointer-fetch surface (what a follower's currency check returns). `None` until the pointer has been
    /// moved; otherwise the raw `WireCurrentRecord` bytes plus the extracted `(epoch, seq)`.
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
    /// bytes. Asserts the scope/path match, R1-authorizes the version read (confirmed member ∧ accepted-trunk
    /// or open-non-stale proposal), and returns the single indistinguishable [`AuthorityError::NotFound`] for an
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
    /// member ∧ `open ∧ base == current` rows, so a staled proposal vanishes exactly as it drops out of the
    /// object/version reads (keep == read == list). A NON-member with a valid credential yields an
    /// EMPTY list, never a not-found (the membership gate is the authz; there is no per-row probe); a scope/path
    /// mismatch is the single indistinguishable [`AuthorityError::NotFound`]. This is a READ — nothing is
    /// moved, no governance is consulted, no body is taken.
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

    /// Resolve a device-lane op's presented workspace credential over the POOL — the shared front door for
    /// every write verb (the transaction re-authenticates in-transaction; this read is advisory for the
    /// pre-txn receipt machinery). `Err(receipt)` is the SYNTHESIZED pre-auth DENIED — never persisted (an
    /// un/deauthenticated caller must mint no durable rows), mirroring the transaction's `denied_preauth`:
    /// - an **UNKNOWN** credential resolves to no row → synthesized DENIED;
    /// - a **REVOKED** credential still RESOLVES (a since-revoked device must be able to replay a stored
    ///   receipt byte-identically), so it is admitted **only when a receipt already exists** for this
    ///   `(ws, device_key_id, op_id)`; genuinely FRESH revoked work has no receipt and is synthesized-DENIED
    ///   here, uniformly closing every pre-txn typed-failure path (bad-digest / op-mismatch / no-current /
    ///   the review-required preflight) as well as the in-txn arm, so a revoked device mints nothing durable.
    async fn resolve_device_op(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        op_id: &OpId,
        auth: &crate::set_current::DeviceOpAuth,
        created_at: &str,
    ) -> Result<std::result::Result<crate::set_current::DeviceOpRequest, SetCurrentReceipt>> {
        let credential_sha256 = topos_core::digest::sha256(auth.credential.as_bytes());
        let Some(identity) = self
            .db()
            .resolve_device_credential(ws, &credential_sha256)
            .await?
        else {
            return Ok(Err(
                self.synthesized_device_denied(skill, op_id, auth, created_at)
            ));
        };
        // A revoked device proceeds ONLY to replay a stored receipt; fresh revoked work mints nothing durable.
        if identity.revoked
            && !self
                .db()
                .device_receipt_exists(ws, &identity.device_key_id, op_id.as_str())
                .await?
        {
            return Ok(Err(
                self.synthesized_device_denied(skill, op_id, auth, created_at)
            ));
        }
        Ok(Ok(crate::set_current::DeviceOpRequest {
            credential_sha256,
            device_key_id: identity.device_key_id,
            op: auth.op,
            expected: auth.expected,
        }))
    }

    /// The SYNTHESIZED (never-persisted) DENIED receipt for an unknown/revoked device on a device-lane
    /// write — the pre-transaction twin of the transaction's `denied_preauth`, so an unauthenticated OR
    /// deauthenticated caller mints nothing durable on ANY path (the in-txn arm and the pre-txn
    /// review-required preflight agree).
    fn synthesized_device_denied(
        &self,
        skill: &SkillId,
        op_id: &OpId,
        auth: &crate::set_current::DeviceOpAuth,
        created_at: &str,
    ) -> SetCurrentReceipt {
        SetCurrentReceipt {
            op_id: op_id.as_str().to_owned(),
            command: crate::set_current::device_op_command(auth.op).to_owned(),
            skill_id: skill.as_str().to_owned(),
            version_id: None,
            bundle_digest: None,
            expected: auth.expected,
            outcome: topos_types::TerminalOutcome::Denied,
            current: None,
            record: None,
            created_at: created_at.to_owned(),
            details: crate::set_current::detail_msg("device unknown or revoked"),
        }
    }

    /// Publish a candidate as the skill's new `current` — a direct publish, or **genesis** for the first
    /// version. The full backbone flow: ingest + migrate (the crash-safe quarantine → lease → install →
    /// record) → the one pure-DB pointer-move transaction (compare-and-set, the catalog/protection gates,
    /// re-root, durable receipt). On an effectively-REVIEWED bundle a plain member's direct publish is
    /// DOWNGRADED in-transaction to a proposal (`NEEDS_REVIEW` + a `downgraded` detail — never a
    /// rejection); reviewer+ lands directly. Returns the durable, replayable receipt; a retry with the
    /// same `op_id` + bound identity returns it byte-identically.
    ///
    /// `display_name` is UNSIGNED ADVISORY metadata (the author's skill-folder name) recorded on the
    /// CATALOG row last-writer-wins — never in the digest, the candidate, or the device-op preimage; at a
    /// genesis it also seeds the catalog NAME. `channel` is the `--to` placement (a channel reference,
    /// created on first use, gated by the channel's mode independently of the version gate); a genesis
    /// with no `channel` is placed in `everyone`.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`]/[`AuthorityError::Integrity`] on a store fault.
    #[allow(clippy::too_many_arguments)]
    pub async fn publish(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        op_id: &OpId,
        candidate: CandidateUpload,
        auth: DeviceOpAuth,
        display_name: Option<&str>,
        channel: Option<&str>,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        // Authenticate FIRST (the pool pre-resolution): an unknown credential mints nothing durable —
        // not even the typed pre-txn receipts below.
        let device = match self
            .resolve_device_op(ws, skill, op_id, &auth, created_at)
            .await?
        {
            Ok(device) => device,
            Err(receipt) => return Ok(receipt),
        };
        // A direct publish must arrive as exactly that. Forwarding an arbitrary device op (e.g. a
        // `Revert`-labelled candidate of new bytes) would skip the direct-publish protection gate while
        // still reaching the promote path — a gate bypass. Reject anything but `PublishDirect` BEFORE
        // ingesting (so a misuse uploads/migrates/leases nothing).
        if !matches!(device.op, crate::set_current::DeviceOp::PublishDirect) {
            return crate::set_current::reject_non_publish_op(
                self, ws, skill, op_id, &device, created_at,
            )
            .await;
        }
        let staged = crate::lifecycle::ingest(self, ws, op_id, candidate, now).await?;
        crate::lifecycle::migrate(self, ws, &staged, now).await?;
        crate::set_current::publish(
            self,
            ws,
            skill,
            &staged,
            &device,
            display_name,
            channel,
            created_at,
            now,
        )
        .await
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
        auth: DeviceOpAuth,
        author: &str,
        message: &str,
        op_id: &OpId,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        let device = match self
            .resolve_device_op(ws, skill, op_id, &auth, created_at)
            .await?
        {
            Ok(device) => device,
            Err(receipt) => return Ok(receipt),
        };
        // A revert must arrive as exactly `Revert` (mirroring the publish/propose/review guards): a
        // mismatched op would otherwise mis-route into the promote arms. Reject it before constructing the
        // forward commit, recording nothing.
        if !matches!(device.op, crate::set_current::DeviceOp::Revert) {
            return crate::set_current::reject_op_mismatch(
                self,
                ws,
                skill,
                op_id,
                &device,
                created_at,
                "a revert must arrive as Revert",
            )
            .await;
        }
        crate::set_current::revert(
            self,
            ws,
            skill,
            good,
            crate::actor::WriteActor::Device {
                credential_sha256: device.credential_sha256,
                device_key_id: &device.device_key_id,
            },
            device.expected,
            author,
            message,
            op_id,
            created_at,
            now,
        )
        .await
    }

    /// Open a **proposal** — upload a candidate version for review WITHOUT moving `current` (the contribute
    /// motion's first half). The full flow: ingest + migrate (the crash-safe quarantine → lease → install →
    /// record, exactly as `publish`), then the one pure-DB transaction opens a `proposals` row and roots the
    /// candidate's bytes through `proposal_object` (gated on `open ∧ non-stale` for both retention and read).
    /// Returns `NEEDS_REVIEW`; `current` is byte-for-byte unchanged and no pointer moves. A later
    /// [`review_approve`](Self::review_approve) promotes it. Genesis cannot be proposed (publish the first
    /// version directly); a `--propose` against a skill with no `current` is a typed failure that uploads nothing.
    ///
    /// `display_name` is UNSIGNED ADVISORY metadata carried for symmetry with [`publish`](Self::publish); a
    /// proposal moves no pointer, so no name is recorded until a later publish/approve advances `current`.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`]/[`AuthorityError::Integrity`] on a store fault.
    #[allow(clippy::too_many_arguments)]
    pub async fn propose(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        op_id: &OpId,
        candidate: CandidateUpload,
        auth: DeviceOpAuth,
        display_name: Option<&str>,
        channel: Option<&str>,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        let device = match self
            .resolve_device_op(ws, skill, op_id, &auth, created_at)
            .await?
        {
            Ok(device) => device,
            Err(receipt) => return Ok(receipt),
        };
        // A proposal must arrive as exactly `PublishPropose`. Forwarding another device op could reach the
        // promote path (which moves `current`) — a gate bypass — so reject anything else BEFORE ingesting (a
        // misuse uploads/migrates/opens nothing).
        if !matches!(device.op, crate::set_current::DeviceOp::PublishPropose) {
            return crate::set_current::reject_op_mismatch(
                self,
                ws,
                skill,
                op_id,
                &device,
                created_at,
                "a proposal must arrive as PublishPropose",
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
        crate::set_current::propose(
            self,
            ws,
            skill,
            &staged,
            &device,
            display_name,
            channel,
            created_at,
            now,
        )
        .await
    }

    /// **Approve** an open proposal — promote it to `current` (the sideways move; the contribute motion's
    /// second half). Uploads/leases/migrates nothing (the candidate is already in the main store, rooted by
    /// its proposal); runs only the one pointer-move transaction, which compare-and-sets on the proposal's
    /// base, performs the `proposal_object → commit_object` handoff, advances the pointer, and flips the
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
        auth: DeviceOpAuth,
        op_id: &OpId,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        let device = match self
            .resolve_device_op(ws, skill, op_id, &auth, created_at)
            .await?
        {
            Ok(device) => device,
            Err(receipt) => return Ok(receipt),
        };
        if !matches!(device.op, crate::set_current::DeviceOp::ReviewApprove) {
            return crate::set_current::reject_op_mismatch(
                self,
                ws,
                skill,
                op_id,
                &device,
                created_at,
                "a review approval must arrive as ReviewApprove",
            )
            .await;
        }
        crate::set_current::review_approve(
            self,
            ws,
            skill,
            commit,
            crate::actor::WriteActor::Device {
                credential_sha256: device.credential_sha256,
                device_key_id: &device.device_key_id,
            },
            device.expected,
            op_id,
            created_at,
            now,
        )
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
        auth: DeviceOpAuth,
        op_id: &OpId,
        created_at: &str,
    ) -> Result<SetCurrentReceipt> {
        let device = match self
            .resolve_device_op(ws, skill, op_id, &auth, created_at)
            .await?
        {
            Ok(device) => device,
            Err(receipt) => return Ok(receipt),
        };
        if !matches!(device.op, crate::set_current::DeviceOp::ReviewReject) {
            return crate::set_current::reject_op_mismatch(
                self,
                ws,
                skill,
                op_id,
                &device,
                created_at,
                "a review rejection must arrive as ReviewReject",
            )
            .await;
        }
        crate::set_current::review_reject(
            self,
            ws,
            skill,
            commit,
            crate::actor::WriteActor::Device {
                credential_sha256: device.credential_sha256,
                device_key_id: &device.device_key_id,
            },
            device.expected,
            None,
            op_id,
            created_at,
        )
        .await
    }

    // ── the web-session REVIEW leg (privileged lib-level, no OSS HTTP route) ─────────────────────────
    //
    // The write twin of the session READ lane: a hosted composition's authenticated admin routes call
    // these with the session's VERIFIED email — the composing caller's session verification is the
    // authentication; the in-transaction gate (a confirmed OWNER or REVIEWER workspace seat — the first
    // enforcement of the reviewer role) is the authorization. The write terminates in the SAME
    // serializable transaction as the device lane (one approve predicate, one CAS, one pointer
    // advance, one four-eyes gate); `plane_mode` is the plane's own posture threaded by the composer,
    // never a request field; self-host is uniformly denied.

    /// **Approve an open proposal from a verified web session** — the browser's "Make current".
    /// `expected` is the generation the caller's rendered diff was computed against; a moved pointer
    /// refuses with the same `CONFLICT` the CLI gets, and the four-eyes gate under `review_required`
    /// applies across lanes (the proposer's canonical email may not approve its own proposal).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`]/[`AuthorityError::Integrity`] on a store fault. Every protocol
    /// outcome is a receipt, not an error.
    #[allow(clippy::too_many_arguments)]
    pub async fn review_approve_session(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        candidate: CommitId,
        expected: topos_types::Generation,
        request_id: &str,
        acting_email: &str,
        plane_mode: DeploymentMode,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        crate::session_review::review_approve_session(
            self,
            ws,
            skill,
            candidate,
            expected,
            request_id,
            acting_email,
            plane_mode,
            created_at,
            now,
        )
        .await
    }

    /// **Reject an open proposal from a verified web session**, with a MANDATORY non-empty `reason`
    /// (recorded on the proposal row; device rejects carry none). `expected` is the proposal's base
    /// generation — reject moves no pointer, so there is no CAS conflict: a mismatched base is the
    /// typed "no open proposal for this candidate and base" denial.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a store fault. Every protocol outcome is a receipt, not an error.
    #[allow(clippy::too_many_arguments)]
    pub async fn review_reject_session(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        candidate: CommitId,
        expected: topos_types::Generation,
        reason: &str,
        request_id: &str,
        acting_email: &str,
        plane_mode: DeploymentMode,
        created_at: &str,
    ) -> Result<SetCurrentReceipt> {
        crate::session_review::review_reject_session(
            self,
            ws,
            skill,
            candidate,
            expected,
            reason,
            request_id,
            acting_email,
            plane_mode,
            created_at,
        )
        .await
    }

    /// **Revert a skill's `current` to a known-good prior version from a verified web session** — the
    /// browser's "Roll back to this version". `good` is the full target commit id; `expected` is the live
    /// `current` generation the caller's version page rendered against (a moved pointer refuses with
    /// `CONFLICT`). Revert bypasses the review gate + four-eyes by design (it restores already-consented
    /// bytes — the safety net); the session gate is the SAME confirmed **owner|reviewer** seat the approve
    /// lane enforces, checked in-transaction (with a cheap pre-stage fence so a plain member never triggers
    /// the forward-commit staging). Followers re-verify the restored bytes against the target's
    /// content-addressed digest — consent stays end-to-end, and the receipt records
    /// `method = web_session` + the acting principal. Idempotent per `request_id` (a canonical UUID).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`]/[`AuthorityError::Integrity`] on a store fault. Every protocol
    /// outcome is a receipt, not an error.
    #[allow(clippy::too_many_arguments)]
    pub async fn revert_session(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        good: CommitId,
        expected: topos_types::Generation,
        request_id: &str,
        acting_email: &str,
        plane_mode: DeploymentMode,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        crate::session_review::revert_session(
            self,
            ws,
            skill,
            good,
            expected,
            request_id,
            acting_email,
            plane_mode,
            created_at,
            now,
        )
        .await
    }

    /// **One proposal's detail for a confirmed member** — the review surface's read: status + base +
    /// the proposer (session-lane-only disclosure; the thin `/v1` listing stays proposer-free) + the
    /// resolution facts + the `review_required` policy at read time. `Ok(None)` — no proposal of this
    /// candidate was ever opened on this skill — is a member-entitled post-gate outcome the composing
    /// wrapper folds into its uniform miss.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] uniformly on every pre-gate miss (self-host / malformed email or
    /// skill or version id / unknown workspace / non-member); [`AuthorityError::Internal`] on a store
    /// fault.
    pub async fn read_proposal_detail_session(
        &self,
        ws: &WorkspaceId,
        skill: &str,
        version_id_hex: &str,
        acting_email: &str,
        plane_mode: DeploymentMode,
    ) -> Result<Option<crate::session_read::ProposalDetailSession>> {
        crate::session_read::read_proposal_detail_session(
            self,
            ws,
            skill,
            version_id_hex,
            acting_email,
            plane_mode,
        )
        .await
    }

    // ── enrollment + governance issuance (every op decided in-Authority; requires with_enrollment_config) ──

    /// Create an owner-driven **invite** — mint the opaque `/i/<token>` link, store it (hash-only), seed the
    /// invited members at `role` (`status = 'invited'`), and record the governance receipt. The acting
    /// device credential is authenticated by registry lookup in-transaction and its principal's OWNER role
    /// enforced there. `op_id`-idempotent: a retry with the matching bound identity re-derives the
    /// IDENTICAL link and replays the receipt; a different request under the same op id is a denied
    /// key-reuse. The role + skills come from `request.op` (a `GovernanceOp::Invite`).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    pub async fn create_invite(
        &self,
        ws: &WorkspaceId,
        op_id: &str,
        request: GovernanceRequest,
        created_at: &str,
        now: i64,
    ) -> Result<CreateInviteOutcome> {
        crate::governance::create_invite(self, ws, op_id, &request, created_at, now).await
    }

    /// Resolve an invite link to its **bootstrap payload** — the workspace identity, the offered skills,
    /// and the API base to enroll against (no bytes, no role). A revoked/expired/absent invite is the
    /// single indistinguishable [`AuthorityError::NotFound`].
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

    /// Start a **STANDUP** device-authorization flow — no invite, no workspace: the session is born
    /// `pending` with `intent = 'standup'`, and a signed-in human's [`approve_standup`](Self::approve_standup)
    /// later creates the workspace it confirms into. CLOUD planes only: on a self-host plane this is the
    /// single indistinguishable [`AuthorityError::NotFound`] (self-host stands up via the operator's
    /// one-time claim link). The standup `user_code` is a HIGH-ENTROPY opaque URL token (the same shape
    /// enroll uses) because the approval CREATES ownership with no roster gate — see the generator's rationale.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on a self-host plane; [`AuthorityError::Internal`] on a fault.
    pub async fn start_standup_device_auth(
        &self,
        device_public_key: &[u8; 32],
        machine_name: &str,
        now: i64,
        created_at: &str,
    ) -> Result<DeviceAuthStart> {
        crate::enroll::start_standup_device_auth(
            self,
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

    /// **Redeem** an enrollment grant — THE central op. The GRANT is the bearer credential (a
    /// deterministic HMAC secret, stored only as its sha256). In one transaction: re-derive the device key
    /// id, check the grant binds exactly this presented device key, apply the membership gate (cloud requires
    /// a confirmed identity; self-host grants membership), and register the device WITH its ONE minted
    /// **workspace credential** (`derive_token(b"wscred", …)`, stored only as its sha256 on the registry
    /// row). **Returns no user token, ever; no per-skill token, ever.** Naturally idempotent — a replay
    /// re-derives the identical workspace credential.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    pub async fn redeem_enrollment(
        &self,
        grant_token: &str,
        device_public_key: [u8; 32],
        now: i64,
        created_at: &str,
    ) -> Result<RedeemOutcome> {
        crate::enroll::redeem_enrollment(self, grant_token, device_public_key, now, created_at)
            .await
    }

    /// **Admin claim** (the one-time first-boot bearer: self-host standup + the cloud break-glass) —
    /// consume the claim token, create the workspace (THE PLANE'S deployment mode; the display name + owner
    /// from the mint-time row, never a request), seat its first owner, and register the claiming device.
    /// Returns an [`EnrollmentRedeemed`](crate::EnrollmentRedeemed)-shaped result. A SAME-DEVICE replay of
    /// an already-consumed claim deterministically re-returns `Redeemed` (lost-200 recovery); every other
    /// dead-claim case is the uniform static denial.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    pub async fn admin_claim(
        &self,
        claim_token: &str,
        device_public_key: [u8; 32],
        now: i64,
        created_at: &str,
    ) -> Result<RedeemOutcome> {
        crate::governance::admin_claim(self, claim_token, device_public_key, now, created_at).await
    }

    /// **Mint** a one-time admin-claim token for a workspace that does not exist yet (typed refusal if it
    /// does). On a CLOUD-mode plane an `owner_email` is REQUIRED (the seated owner must be a governable
    /// human identity); self-host may omit it (the claiming device roots the owner). The plaintext token is
    /// returned ONCE — only its sha256 is stored, and the result's `Debug` redacts it; the caller must
    /// never log or trace it. Re-minting for the same absent workspace is allowed (first redeem wins).
    ///
    /// This is a PRIVILEGED lib-level op (no OSS HTTP route): the bin's `mint-claim` subcommand and a
    /// composing plane's admin surface call it.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    #[allow(clippy::too_many_arguments)]
    pub async fn mint_admin_claim(
        &self,
        ws: &WorkspaceId,
        display_name: Option<&str>,
        owner_email: Option<&str>,
        plane_mode: DeploymentMode,
        ttl_ms: i64,
        now: i64,
        created_at: &str,
    ) -> Result<MintClaimOutcome> {
        crate::governance::mint_admin_claim(
            self,
            ws,
            display_name,
            owner_email,
            plane_mode,
            ttl_ms,
            now,
            created_at,
        )
        .await
    }

    /// **Create a workspace** for an already-verified owner email (the self-serve door a composing web
    /// surface drives; the caller MUST have proven the email). ONE transaction: the `request_id` idempotency
    /// probe (same request + same owner replays the SAME workspace and self-invite link; a different owner
    /// is denied), the per-identity creation cap, a fresh server-minted `w_…` id, the workspace + confirmed
    /// owner seat (with the freemail-aware domain claim), and the owner's deterministic self-invite.
    /// `display_name = None` takes the server default (the email's local part + "'s workspace").
    ///
    /// This is a PRIVILEGED lib-level op (no OSS HTTP route); `plane_mode` is the plane's own posture,
    /// threaded by the composing caller — never a request field.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    pub async fn create_workspace(
        &self,
        request_id: &str,
        display_name: Option<&str>,
        owner_email: &str,
        plane_mode: DeploymentMode,
        created_at: &str,
    ) -> Result<CreateWorkspaceOutcome> {
        crate::governance::create_workspace(
            self,
            request_id,
            display_name,
            owner_email,
            plane_mode,
            created_at,
        )
        .await
    }

    /// **Approve a standup session** with a web-verified email (the human leg of the un-enrolled publish
    /// door; the caller MUST have proven the email). ONE transaction: resolve the live standup session by
    /// `user_code`, run the same creation body as [`create_workspace`](Self::create_workspace) (cap → fresh
    /// id → seat), and CAS the session pending→confirmed with the fresh workspace — the session CAS is the
    /// idempotency (a same-email re-click is `AlreadyApproved`; a different email, an unknown/expired code,
    /// or an enroll-intent session is the single indistinguishable [`AuthorityError::NotFound`]).
    ///
    /// This is a PRIVILEGED lib-level op (no OSS HTTP route).
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on the uniform miss; [`AuthorityError::Internal`] on a fault.
    pub async fn approve_standup(
        &self,
        user_code: &str,
        verified_email: &str,
        display_name: Option<&str>,
        plane_mode: DeploymentMode,
        now: i64,
        created_at: &str,
    ) -> Result<ApproveStandupOutcome> {
        crate::governance::approve_standup(
            self,
            user_code,
            verified_email,
            display_name,
            plane_mode,
            now,
            created_at,
        )
        .await
    }

    /// **Invite members from a verified owner session** — the hosted cloud's "add teammates in settings"
    /// leg (the composing WEB layer proves the acting email; this op never does). ONE transaction: the
    /// `request_id` idempotency slot (the same `workspace_events` slot the device lane uses, under a fresh
    /// session-tagged identity — a cross-leg id collision always fails closed), the in-transaction acting
    /// gate (the acting email must hold a CONFIRMED **owner** seat — one uniform denial otherwise), the
    /// standing-door ensure (lazily minted for door-less workspaces), and the invited seats seeded at
    /// `role` through the shared never-demote row-writer. Returns the standing door token (compose
    /// `<link_base>/i/<token>`); an owner-role request is unrepresentable in [`SessionInviteRole`].
    ///
    /// This is a PRIVILEGED lib-level op (no OSS HTTP route); `plane_mode` is the plane's own posture,
    /// threaded by the composing caller — never a request field. ALL session roster ops are uniformly
    /// denied on a self-host plane (self-host membership stays the device invite chain).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    #[allow(clippy::too_many_arguments)]
    pub async fn invite_members_session(
        &self,
        ws: &WorkspaceId,
        request_id: &str,
        acting_email: &str,
        emails: &[String],
        role: SessionInviteRole,
        plane_mode: DeploymentMode,
        created_at: &str,
        now: i64,
    ) -> Result<SessionInviteOutcome> {
        crate::session_roster::invite_members_session(
            self,
            ws,
            request_id,
            acting_email,
            emails,
            role,
            plane_mode,
            created_at,
            now,
        )
        .await
    }

    /// **Remove a member from a verified owner session.** Same acting gate + idempotency shape as
    /// [`invite_members_session`](Self::invite_members_session); reuses the device lane's
    /// last-owner-lockout guard and its exact removal transaction: the lapse-detach reconcile runs
    /// first (the entitlement union is membership-gated, so it must read while the seat still
    /// stands), then the `workspace_member` seat is dropped — every read/write gate joins that seat,
    /// so removal severs access the instant it commits, while the device's credential row stays
    /// (gating nothing without a seat; re-adding the member re-enables the same devices).
    /// Removing a merely-invited seat un-invites it; removing an absent principal is an idempotent `Ok`.
    ///
    /// This is a PRIVILEGED lib-level op (no OSS HTTP route); uniformly denied on self-host.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    #[allow(clippy::too_many_arguments)]
    pub async fn roster_remove_session(
        &self,
        ws: &WorkspaceId,
        request_id: &str,
        acting_email: &str,
        target_email: &str,
        plane_mode: DeploymentMode,
        created_at: &str,
        now: i64,
    ) -> Result<GovernanceOutcome> {
        crate::session_roster::roster_remove_session(
            self,
            ws,
            request_id,
            acting_email,
            target_email,
            plane_mode,
            created_at,
            now,
        )
        .await
    }

    /// **Rotate the standing workspace door from a verified owner session** — "reset link". Revokes the
    /// current door family (the epoch door AND the create-page genesis self-invite, whichever stand),
    /// bumps the workspace's `link_epoch`, and mints the new deterministic door. Blocks FUTURE redemption
    /// only: an already-exchanged credential (or a device-auth session already past its entry gate) is
    /// never severed, and invite links minted on the device leg are deliberately untouched.
    ///
    /// This is a PRIVILEGED lib-level op (no OSS HTTP route); uniformly denied on self-host.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    #[allow(clippy::too_many_arguments)]
    pub async fn rotate_join_link_session(
        &self,
        ws: &WorkspaceId,
        request_id: &str,
        acting_email: &str,
        plane_mode: DeploymentMode,
        created_at: &str,
        now: i64,
    ) -> Result<SessionRotateOutcome> {
        crate::session_roster::rotate_join_link_session(
            self,
            ws,
            request_id,
            acting_email,
            plane_mode,
            created_at,
            now,
        )
        .await
    }

    /// **Read the workspace roster for a verified session** — a pure privileged read (no receipt): every
    /// seat (email, role, invited/confirmed status, added-at) for any CONFIRMED member; the standing door
    /// token included ONLY when the acting email holds a confirmed **owner** seat (`None` also when no
    /// door stands yet). Every miss — a self-host plane, an absent workspace, a non-member — is the single
    /// indistinguishable [`AuthorityError::NotFound`].
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on the uniform miss; [`AuthorityError::Internal`] on a fault.
    pub async fn read_roster(
        &self,
        ws: &WorkspaceId,
        acting_email: &str,
        plane_mode: DeploymentMode,
    ) -> Result<RosterView> {
        crate::session_roster::read_roster(self, ws, acting_email, plane_mode).await
    }

    // ── the web-session READ lane (privileged lib-level, no OSS HTTP route) ─────────────────────────
    //
    // The read twin of the session-roster leg: a hosted composition's authenticated admin routes call
    // these with the session's VERIFIED email — the composing caller's session verification is the
    // authentication. `plane_mode` is the plane's own posture threaded by the composer, never a request
    // field. Authorization is a CONFIRMED `workspace_member` row (any role — deliberately broader than
    // the device lane's per-skill roster: catalog visibility IS workspace membership); self-host,
    // malformed inputs, unknown workspaces, non-members, and invited-but-unconfirmed seats are ALL the
    // single indistinguishable [`AuthorityError::NotFound`] (the shared `member_gate` preamble is the one
    // session entry). Reads mint nothing durable — no receipts, no `workspace_events`.

    /// The workspace catalog for a confirmed member: every skill holding a `current` row, with its
    /// pointer generation, epoch-ms update time, consent `bundle_digest`, and OPEN non-stale proposal
    /// count (delegated per skill to the same listing statement, so count == list by construction).
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] per the lane's uniformity rule above; [`AuthorityError::Integrity`]
    /// on a corrupt stored row; [`AuthorityError::Internal`] on a database fault.
    pub async fn list_skills_session(
        &self,
        ws: &WorkspaceId,
        acting_email: &str,
        plane_mode: DeploymentMode,
    ) -> Result<Vec<SkillIndexRow>> {
        crate::session_read::list_skills_session(self, ws, acting_email, plane_mode).await
    }

    /// The workspace catalog for a DEVICE-credential member read (`list --remote`) — the catalog-visibility
    /// twin of [`list_skills_session`](Self::list_skills_session), authorized WITHOUT a web session and
    /// available on BOTH cloud and self-host (device auth IS the self-host membership story, so this op
    /// does **not** take or consult a [`DeploymentMode`]). Authorized iff the device is a NON-REVOKED
    /// registered device (the credential lookup IS the authentication) and its bound principal is a
    /// CONFIRMED workspace member. Every unknown/revoked
    /// device, or non-member, is the single indistinguishable [`AuthorityError::NotFound`]. A
    /// pool read — no transaction, no receipt, no op id. `now` is accepted for call-shape parity with the
    /// token-lane reads (device registration carries no expiry to enforce).
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] per the uniformity rule above; [`AuthorityError::Integrity`] on a
    /// corrupt stored row; [`AuthorityError::Internal`] on a database fault.
    pub async fn list_skills_device(
        &self,
        ws: &WorkspaceId,
        credential: &str,
        now: i64,
    ) -> Result<Vec<SkillIndexRow>> {
        crate::session_read::list_skills_device(self, ws, credential, now).await
    }

    /// **The delivery read** — what THIS device should have: the ONE entitlement predicate
    /// (roster-derived `everyone` ∪ followed channels ∪ direct follows − unfollows − this device's
    /// exclusions, active + current-holding skills only) plus the person's detached skills
    /// (freeze-in-place), the unacked notices feed, and the open-proposal count. Device-lane
    /// authenticated (Bearer workspace credential); every miss — unknown/revoked credential,
    /// non-member (a REMOVED member reads this for the whole workspace) — is the uniform
    /// [`AuthorityError::NotFound`]. A pool read; mints nothing durable.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] per the lane's uniformity rule; [`AuthorityError::Integrity`] on
    /// a corrupt stored row; [`AuthorityError::Internal`] on a database fault.
    pub async fn delivery(&self, ws: &WorkspaceId, credential: &str) -> Result<Delivery> {
        crate::delivery::delivery(self, ws, credential).await
    }

    /// **The applied-state report** — the fleet's visibility write: upsert this device's
    /// `(skill, applied version)` snapshot, drop the non-detached rows it no longer names, stamp
    /// `last_report_at` (the staleness clock). Detach records are never touched. Same
    /// authentication + uniformity as [`delivery`](Self::delivery); last-writer-wins (no receipt).
    ///
    /// # Errors
    /// As [`delivery`](Self::delivery).
    pub async fn report_applied(
        &self,
        ws: &WorkspaceId,
        credential: &str,
        applied: &[AppliedSkill],
        now: i64,
    ) -> Result<()> {
        crate::delivery::report_applied(self, ws, credential, applied, now).await
    }

    /// **Curation: place a skill reference into a channel** (`channel add` / the standalone
    /// `publish --to` twin) — creating the channel on first use (member-level self-serve); a
    /// `curated` channel takes reviewer+. Device-lane authenticated; misses uniform; role/mode
    /// refusals typed (an authenticated member is entitled to the reason). Idempotent; the audit
    /// trail is trigger-emitted.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on the uniform miss; [`AuthorityError::Internal`] on a fault.
    pub async fn channel_place(
        &self,
        ws: &WorkspaceId,
        credential: &str,
        channel: &str,
        skill_name: &str,
        created_at: &str,
    ) -> Result<CurationOutcome> {
        crate::channels::channel_place(self, ws, credential, channel, skill_name, created_at).await
    }

    /// **Curation: remove a skill reference from a channel** — symmetric gate with
    /// [`channel_place`](Self::channel_place); the channel and the skill keep existing either way.
    ///
    /// # Errors
    /// As [`channel_place`](Self::channel_place).
    pub async fn channel_unplace(
        &self,
        ws: &WorkspaceId,
        credential: &str,
        channel: &str,
        skill_name: &str,
        created_at: &str,
    ) -> Result<CurationOutcome> {
        crate::channels::channel_unplace(self, ws, credential, channel, skill_name, created_at)
            .await
    }

    /// **Join a channel** — always self-serve for a confirmed member; `everyone` refuses (its
    /// membership IS the roster). Joining re-attaches any detach records the new entitlements
    /// cover.
    ///
    /// # Errors
    /// As [`channel_place`](Self::channel_place).
    pub async fn channel_join(
        &self,
        ws: &WorkspaceId,
        credential: &str,
        channel: &str,
        created_at: &str,
    ) -> Result<ChannelMembershipOutcome> {
        crate::channels::channel_join(self, ws, credential, channel, created_at).await
    }

    /// **Leave a channel** — self-serve; `everyone` cannot be left. Runs the lapse-detach
    /// reconcile: skills the person now receives from NO source get their final per-device detach
    /// records (reference counting via the union — a skill another followed channel still
    /// references stays live).
    ///
    /// # Errors
    /// As [`channel_place`](Self::channel_place).
    pub async fn channel_leave(
        &self,
        ws: &WorkspaceId,
        credential: &str,
        channel: &str,
        now: i64,
        created_at: &str,
    ) -> Result<ChannelMembershipOutcome> {
        crate::channels::channel_leave(self, ws, credential, channel, now, created_at).await
    }

    /// **Direct-follow a skill** (person-scoped; by its catalog NAME): survives any channel
    /// dropping it; clears the person's unfollow mask + THIS device's exclusion; re-attaches
    /// detach records. An archived skill refuses typed (a freed name is a new identity).
    ///
    /// # Errors
    /// As [`channel_place`](Self::channel_place).
    pub async fn follow_skill(
        &self,
        ws: &WorkspaceId,
        credential: &str,
        skill_name: &str,
        created_at: &str,
    ) -> Result<SubscriptionOutcome> {
        crate::channels::follow_skill(self, ws, credential, skill_name, created_at).await
    }

    /// **Unfollow a skill** (person-scoped): the standing negative mask — delivery ends on ALL the
    /// person's devices however many channels reference it — plus the final per-device detach
    /// records. `follow` re-attaches.
    ///
    /// # Errors
    /// As [`channel_place`](Self::channel_place).
    pub async fn unfollow_skill(
        &self,
        ws: &WorkspaceId,
        credential: &str,
        skill_name: &str,
        now: i64,
        created_at: &str,
    ) -> Result<SubscriptionOutcome> {
        crate::channels::unfollow_skill(self, ws, credential, skill_name, now, created_at).await
    }

    /// **Exclude a followed skill from THIS device** ("not on this device" — the `remove` verb's
    /// row): other devices keep receiving; `follow` on this device lifts it.
    ///
    /// # Errors
    /// As [`channel_place`](Self::channel_place).
    pub async fn exclude_device(
        &self,
        ws: &WorkspaceId,
        credential: &str,
        skill_name: &str,
        created_at: &str,
    ) -> Result<SubscriptionOutcome> {
        crate::channels::exclude_device(self, ws, credential, skill_name, created_at).await
    }

    /// **The `protect` setter** — per-bundle protection (`reviewed`) or per-channel mode
    /// (`curated`), kind-polymorphic. Tightening takes reviewer+; loosening back to `open` widens
    /// what members can do and takes an owner (pinning `open` explicitly is also an owner act — it
    /// opts the bundle out of a future default tightening). Pending proposals survive a loosening.
    ///
    /// # Errors
    /// As [`channel_place`](Self::channel_place).
    pub async fn protect(
        &self,
        ws: &WorkspaceId,
        credential: &str,
        kind: ProtectKind,
        target_name: &str,
        level: ProtectLevel,
        created_at: &str,
    ) -> Result<ProtectOutcome> {
        crate::channels::protect(self, ws, credential, kind, target_name, level, created_at).await
    }

    /// **Join a channel from a verified session** (the web "join" button's row op) — the same
    /// guarded function as the device lane, gated by the same membership predicate.
    ///
    /// # Errors
    /// As [`channel_place`](Self::channel_place).
    pub async fn channel_join_session(
        &self,
        ws: &WorkspaceId,
        acting_email: &str,
        channel: &str,
        created_at: &str,
    ) -> Result<ChannelMembershipOutcome> {
        crate::channels::channel_join_session(self, ws, acting_email, channel, created_at).await
    }

    /// **Leave a channel from a verified session** — the session twin of
    /// [`channel_leave`](Self::channel_leave), lapse-detach reconcile included.
    ///
    /// # Errors
    /// As [`channel_place`](Self::channel_place).
    pub async fn channel_leave_session(
        &self,
        ws: &WorkspaceId,
        acting_email: &str,
        channel: &str,
        now: i64,
        created_at: &str,
    ) -> Result<ChannelMembershipOutcome> {
        crate::channels::channel_leave_session(self, ws, acting_email, channel, now, created_at)
            .await
    }

    /// **Archive a skill from a verified OWNER session** (web-surface class; self-host denied like
    /// every session op): renames the catalog entry (`<name>-archived-<date>`) FREEING the base
    /// name, removes it from every channel, closes open proposals with author notices, and drops it
    /// out of delivery. Unfollowed/detached copies are never touched.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on the uniform pre-gate miss; [`AuthorityError::Internal`] on a
    /// fault.
    pub async fn archive_skill_session(
        &self,
        ws: &WorkspaceId,
        acting_email: &str,
        skill_name: &str,
        plane_mode: DeploymentMode,
        created_at: &str,
        now: i64,
    ) -> Result<LifecycleOutcome> {
        crate::catalog::archive_skill_session(
            self,
            ws,
            acting_email,
            skill_name,
            plane_mode,
            created_at,
            now,
        )
        .await
    }

    /// **Unarchive** — renames back if the base name is still free, else the typed `NameTaken`
    /// refusal; channel placements are NOT restored (curation moved on).
    ///
    /// # Errors
    /// As [`archive_skill_session`](Self::archive_skill_session).
    pub async fn unarchive_skill_session(
        &self,
        ws: &WorkspaceId,
        acting_email: &str,
        skill_name: &str,
        plane_mode: DeploymentMode,
    ) -> Result<LifecycleOutcome> {
        crate::catalog::unarchive_skill_session(self, ws, acting_email, skill_name, plane_mode)
            .await
    }

    /// **Delete an archived skill** (archive-first; the catalog row is the tombstone under its
    /// archived name): un-roots every version's content for the GC. Deletion cannot recall device
    /// copies — the fleet page names who still holds them.
    ///
    /// # Errors
    /// As [`archive_skill_session`](Self::archive_skill_session).
    pub async fn delete_skill_session(
        &self,
        ws: &WorkspaceId,
        acting_email: &str,
        skill_name: &str,
        plane_mode: DeploymentMode,
        now: i64,
    ) -> Result<LifecycleOutcome> {
        crate::catalog::delete_skill_session(self, ws, acting_email, skill_name, plane_mode, now)
            .await
    }

    /// **Purge ONE version's bytes** (the leak tool): refused while it is `current`; the hash stays
    /// in history as a tombstone (who, when); only blobs unreachable from any live version drop
    /// out; dependent proposals close with author notices. The reclaim itself rides the next GC
    /// pass.
    ///
    /// # Errors
    /// As [`archive_skill_session`](Self::archive_skill_session).
    #[allow(clippy::too_many_arguments)]
    pub async fn purge_version_session(
        &self,
        ws: &WorkspaceId,
        acting_email: &str,
        skill_name: &str,
        version: CommitId,
        plane_mode: DeploymentMode,
        created_at: &str,
        now: i64,
    ) -> Result<PurgeOutcome> {
        crate::catalog::purge_version_session(
            self,
            ws,
            acting_email,
            skill_name,
            version,
            plane_mode,
            created_at,
            now,
        )
        .await
    }

    /// A skill's `current` pointer for a confirmed member. `Ok(None)` — no pointer record exists
    /// for this (ws, skill): a cataloged-but-recordless skill and an unknown skill id are deliberately
    /// indistinguishable here; the composing wrapper folds both into the uniform miss — is a
    /// member-entitled post-gate outcome, distinct from this layer's uniform `NotFound`.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] per the lane's uniformity rule; [`AuthorityError::Integrity`] if the
    /// stored record is corrupt; [`AuthorityError::Internal`] on a database fault.
    pub async fn read_current_session(
        &self,
        ws: &WorkspaceId,
        skill: &str,
        acting_email: &str,
        plane_mode: DeploymentMode,
    ) -> Result<Option<CurrentPointer>> {
        crate::session_read::read_current_session(self, ws, skill, acting_email, plane_mode).await
    }

    /// One object's bytes for a confirmed member — the same verify-on-read fetch, gate/reach
    /// authorization, and re-authorize-on-miss guard as the token lane (reclaimed ⇒ 404, corruption ⇒
    /// Integrity), with the member lane as the gate.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] per the lane's uniformity rule (including a malformed object id);
    /// [`AuthorityError::Integrity`] on provenance/store divergence; [`AuthorityError::Internal`] on a
    /// database fault.
    pub async fn serve_object_session(
        &self,
        ws: &WorkspaceId,
        skill: &str,
        object_id_hex: &str,
        acting_email: &str,
        plane_mode: DeploymentMode,
    ) -> Result<Vec<u8>> {
        crate::session_read::serve_object_session(
            self,
            ws,
            skill,
            object_id_hex,
            acting_email,
            plane_mode,
        )
        .await
    }

    /// A version's authenticated metadata for a confirmed member — the same R1 authorization shape as the
    /// token lane's version read (an unaccepted/rejected/staled candidate is the uniform miss).
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] per the lane's uniformity rule; [`AuthorityError::Integrity`] on a
    /// provenance divergence; [`AuthorityError::Internal`] on a database fault.
    pub async fn read_version_metadata_session(
        &self,
        ws: &WorkspaceId,
        skill: &str,
        version_id_hex: &str,
        acting_email: &str,
        plane_mode: DeploymentMode,
    ) -> Result<VersionMeta> {
        crate::session_read::read_version_metadata_session(
            self,
            ws,
            skill,
            version_id_hex,
            acting_email,
            plane_mode,
        )
        .await
    }

    /// The OPEN, non-stale proposals on one skill for a confirmed member — the same staleness predicate
    /// as the token lane's listing (keep == read == list), with the member lane as the gate.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] per the lane's uniformity rule; [`AuthorityError::Integrity`] on a
    /// corrupt stored row; [`AuthorityError::Internal`] on a database fault.
    pub async fn list_open_proposals_session(
        &self,
        ws: &WorkspaceId,
        skill: &str,
        acting_email: &str,
        plane_mode: DeploymentMode,
    ) -> Result<Vec<OpenProposalSummary>> {
        crate::session_read::list_open_proposals_session(self, ws, skill, acting_email, plane_mode)
            .await
    }

    /// Resolve an admin-claim link token to its **bootstrap payload** (the `/i/` claim branch): the
    /// workspace-to-be's identity from the claim row, no skills, `enrollment_method = "admin_claim"`, and
    /// the enrollment method + API base. A consumed/expired/unknown claim is the single indistinguishable
    /// [`AuthorityError::NotFound`]. Claim resolution never touches the invites table (nor vice versa).
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on a dead/unknown claim; [`AuthorityError::Internal`] on a fault.
    pub async fn read_claim_bootstrap(&self, token: &str, now: i64) -> Result<InviteBootstrap> {
        crate::governance::read_claim_bootstrap(self, token, now).await
    }

    /// **Set** a principal's workspace role (owner-only governance op, with the last-owner-lockout guard).
    /// Device-credential authenticated + `op_id`-idempotent. The role + target come from `request.op` (a `GovernanceOp::RosterSet`).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    pub async fn roster_set(
        &self,
        ws: &WorkspaceId,
        op_id: &str,
        request: GovernanceRequest,
        created_at: &str,
        now: i64,
    ) -> Result<GovernanceOutcome> {
        if !matches!(request.op, GovernanceOp::RosterSet { .. }) {
            return Ok(GovernanceOutcome::Denied("op is not a roster_set"));
        }
        crate::governance::governance_mutation(self, ws, op_id, &request, created_at, now).await
    }

    /// **Remove** a principal from the workspace roster (owner-only, with the last-owner-lockout guard).
    /// Device-credential authenticated + `op_id`-idempotent. The target comes from `request.op` (a `GovernanceOp::RosterRemove`).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    pub async fn roster_remove(
        &self,
        ws: &WorkspaceId,
        op_id: &str,
        request: GovernanceRequest,
        created_at: &str,
        now: i64,
    ) -> Result<GovernanceOutcome> {
        if !matches!(request.op, GovernanceOp::RosterRemove { .. }) {
            return Ok(GovernanceOutcome::Denied("op is not a roster_remove"));
        }
        crate::governance::governance_mutation(self, ws, op_id, &request, created_at, now).await
    }

    /// **Revoke** a registered device — flip `revoked` in one transaction (instant
    /// per-device revoke). Owner OR the device's own principal may act. Device-credential authenticated +
    /// `op_id`-idempotent. The target device key id comes from `request.op` (a `GovernanceOp::DeviceRevoke`).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is set; a database fault.
    pub async fn revoke_device(
        &self,
        ws: &WorkspaceId,
        op_id: &str,
        request: GovernanceRequest,
        created_at: &str,
        now: i64,
    ) -> Result<GovernanceOutcome> {
        if !matches!(request.op, GovernanceOp::DeviceRevoke { .. }) {
            return Ok(GovernanceOutcome::Denied("op is not a device_revoke"));
        }
        crate::governance::governance_mutation(self, ws, op_id, &request, created_at, now).await
    }

    /// Every workspace currently holding stored objects (an `object_presence` row exists) — the enumeration
    /// the composing server drives its periodic per-workspace [`run_gc`](Self::run_gc) over (the recovery
    /// sweep and janitor enumerate cross-workspace internally). GC acts only on objects with a presence row,
    /// so a workspace absent here has nothing a pass could reclaim; ids only (no names, no bytes, no roster
    /// facts — a scheduling surface, not a read).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault; [`AuthorityError::Integrity`] on a corrupt stored id.
    pub async fn workspaces(&self) -> Result<Vec<WorkspaceId>> {
        self.db.workspaces_with_objects().await
    }

    /// Run one **garbage-collection pass** over a workspace: reclaim every currently-unrooted object through
    /// the transactional mark-then-claim fence (claim → unlink-outside-any-transaction → finalize; the
    /// keep-set is exactly the read-authorization surface, so a readable object is never reclaimed). Returns
    /// the number of objects reclaimed. `now` is the server clock in epoch **milliseconds**.
    ///
    /// **The composing server owns scheduling** — this library holds no scheduler. Run it on startup and
    /// periodically (≈ every few minutes) per active workspace; without it, storage abandoned by rejected/
    /// stale proposals and crashed migrates grows without bound.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database/store fault; [`AuthorityError::Integrity`] on a corrupt row.
    pub async fn run_gc(&self, ws: &WorkspaceId, now: i64) -> Result<usize> {
        crate::gc::run_gc(self, ws, now).await
    }

    /// Run the **recovery sweep**: finalize every STALE `deleting` object across all workspaces (a crashed
    /// GC's leftover mid-unlink), re-verifying the read-authorization surface at delete time so a re-rooted
    /// row is spared. Returns the number recovered. `now` is the server clock in epoch **milliseconds**.
    ///
    /// **The composing server owns scheduling** — run it on startup and periodically (≈ every few minutes),
    /// or a stranded `deleting` row makes every migrate of that content wait out its bound and fail.
    ///
    /// # Errors
    /// As [`run_gc`](Self::run_gc).
    pub async fn run_recovery(&self, now: i64) -> Result<usize> {
        crate::gc::recovery_sweep(self, now).await
    }

    /// Run the **quarantine janitor**: sweep every expired/abandoned upload quarantine across all workspaces
    /// (claim-before-rm, so a re-ingest that reused an op id is never swept out from under its in-flight
    /// migrate). Returns the number swept. `now` is the server clock in epoch **milliseconds**.
    ///
    /// **The composing server owns scheduling** — run it on startup and periodically (the quarantine TTL is
    /// generous, so hourly is plenty).
    ///
    /// # Errors
    /// As [`run_gc`](Self::run_gc).
    pub async fn run_janitor(&self, now: i64) -> Result<usize> {
        crate::gc::quarantine_janitor(self, now).await
    }

    /// Set the workspace's `review_required` policy — the off-by-default anti-poisoning gate. With it on, a
    /// direct publish short-circuits to `APPROVAL_REQUIRED` (ingesting nothing) and an approval requires a
    /// second, distinct reviewer (four-eyes); genesis + revert bypass it. This is the authorized public op a
    /// downstream plane (or its admin console) toggles; the test-only `seed_review_required` shim delegates
    /// to it. A trusted caller (the toggle carries no device credential of its own — the device-credential governance
    /// route over this policy is later work); authorization to call it is the composing plane's concern.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn set_review_required(&self, ws: &WorkspaceId, review_required: bool) -> Result<()> {
        self.db.set_review_required(ws, review_required).await
    }

    /// The enrollment-config disclosure (API base URL / deployment posture / enrollment method) — what a
    /// standup `device/authorize` response carries as its plane block, from the ONE authoritative copy
    /// (the enrollment config this authority was built with).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no enrollment config is configured.
    pub fn enrollment_disclosure(&self) -> Result<EnrollmentDisclosure> {
        let config = &self.enrollment()?.config;
        Ok(EnrollmentDisclosure {
            base_url: config.base_url.clone(),
            link_base: config.link_base().to_owned(),
            deployment_mode: config.deployment_mode,
            enrollment_method: config.enrollment_method.clone(),
        })
    }

    // ── pub(crate) internals the port modules drive ──────────────────────────────────────────────

    /// The Postgres backend handle (raw SQL stays inside `mod db`).
    pub(crate) fn db(&self) -> &Db {
        &self.db
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
    /// `pub(crate)` so a blocking-pool closure (which cannot capture `&Authority` — `spawn_blocking`
    /// requires `'static`) can carry the owned path and open the non-`Send` store inside itself.
    pub(crate) fn workspace_git_dir(&self, ws: &WorkspaceId) -> PathBuf {
        self.git_root.join(ws.as_str())
    }

    /// The per-op upload-quarantine directory: `git_root/<ws>.quarantine/<op_id>`. `WorkspaceId` forbids
    /// `.`, so `<ws>.quarantine` can never collide with a real workspace store dir (`git_root/<ws>`), and
    /// it is a SIBLING of that store — so the GC scanner, which walks only `git_root/<ws>/`, never sees a
    /// quarantine. Both ids are validated path-safe newtypes, so the path can never escape `git_root`.
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
    /// on a workspace's first upload). Delegates to the free [`open_or_init_store`], which a blocking-pool
    /// closure calls directly with the owned dir (it cannot capture `&Authority`).
    pub(crate) fn store_for_write(&self, ws: &WorkspaceId) -> Result<Store> {
        open_or_init_store(&self.workspace_git_dir(ws))
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

/// Open-or-create a bare per-workspace git store at `dir` — the write-path open. A free fn so a
/// blocking-pool closure can call it with an owned dir.
///
/// Creation is serialized under an in-process lock: two concurrent first-time writers can both observe
/// the directory as absent, and bare-repo `init` is neither an idempotent open-or-create nor atomic (a
/// racer can open a repo mid-init and fail) — write sections now genuinely run in parallel on the
/// blocking pool, so a bare open→init would race. Under the lock the loser re-opens what the winner
/// completed; the fast path (the store already exists) takes no lock at all. The lock covers ONE
/// process; the `or_else(open)` below covers the rest.
pub(crate) fn open_or_init_store(dir: &Path) -> Result<Store> {
    if let Ok(store) = Store::open(dir) {
        return Ok(store);
    }
    static INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _creation = INIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match Store::open(dir) {
        Ok(store) => Ok(store),
        // The CROSS-PROCESS creation race: two plane processes sharing one git volume (a rolling
        // deploy's overlap, a second replica on a shared mount) can both attempt first-time creation,
        // and the in-process mutex is no help there. If our `init` lost to another process's completed
        // `init`, fall back to opening what that process created rather than failing the write.
        Err(_) => Store::init(dir)
            .or_else(|_| Store::open(dir))
            .map_err(AuthorityError::internal),
    }
}

/// Run one synchronous store section on tokio's **blocking pool**, so fsync-heavy git/large-object I/O
/// (bundle staging, durable installs and commits, verify-on-read byte fetches up to the reject cap) never
/// pins an async worker thread — a few concurrent large operations would otherwise stall every cheap route
/// (the 304 currency check each agent session fires). The closure takes **owned** inputs and opens the
/// non-`Send` gix `Store` inside itself (it can never cross the boundary); a pool-join fault maps to
/// [`AuthorityError::Internal`].
pub(crate) async fn run_blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(AuthorityError::internal)?
}
