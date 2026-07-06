//! `plane-store` — the server authority boundary.
//!
//! A crate **so that raw access is private.** It owns the plane's per-workspace SQL (raw `sqlx`, no
//! ORM) and per-workspace git-object storage, and it is the single place the **skill-scoped access
//! rule** is enforced. The pool, every transaction, every raw SQL statement, and every raw git-object
//! read are `pub(crate)`-private; the **only** public surface is authorized authority operations on
//! [`Authority`]. No code outside this crate can run an unbound query or read a bare object — that
//! privacy boundary *is* the enforcement mechanism (misuse-prevention by encapsulation, not isolation
//! of malicious same-process code).
//!
//! ## What this layer does today
//!
//! - **Per-workspace storage, hard tenant binding.** One git object store per workspace under a
//!   confined root, plus a Postgres database whose every row carries `workspace_id` and whose every query
//!   binds it. Cross-company is physically separate; isolation is the database binding, never the
//!   directory.
//! - **The skill-scoped object read** ([`Authority::read_object`]). One join authorizes on two
//!   independent facts — the caller is rostered for the skill, and some commit of that skill reaches
//!   the object — and yields a witness commit; the bytes are then read + re-verified from the store.
//!   Every not-entitled/not-found case returns one indistinguishable not-found; objects are never
//!   served by bare hash.
//! - **The pointer-move writes** ([`Authority::publish`] / [`Authority::revert`]) and the **contribute
//!   writes** ([`Authority::propose`] / [`Authority::review_approve`] / [`Authority::review_reject`]). A
//!   candidate is always ingested + migrated (full-tree upload, server rehash — no client id is trusted, no
//!   reference-by-id — the canonical rules, a GC-excluded quarantine, lease-before-migrate, server-side
//!   dedup, durable install), then one serializable pure-DB transaction advances `current` under a
//!   whole-`(epoch, seq)` compare-and-set (publish/revert/approve) or opens a gated proposal (propose),
//!   signs the new pointer, and writes a durable all-outcome receipt. `commit_object` is written ONLY by the
//!   accepted-trunk path (publish/revert/approve); a proposal roots its bytes through `proposal_object`,
//!   gated for both retention and read on `open ∧ non-stale`.
//! - **The cross-skill lineage predicate** ([`Authority::check_lineage`]) — a read-only gather + the pure
//!   decision; the pointer-move enforces the same rule transactionally.
//! - **The DB-authoritative object-lifecycle / garbage-collection fence.** A GC-excluded upload
//!   quarantine; the fenced `object_presence` compare-and-swap state machine
//!   (`present`/`deleting`/`absent`/`unavailable`, a `deleting` object non-resurrectable); promotion leases
//!   that root a commit's full object set before any byte migrates; migrate-into-git (lease-before-migrate,
//!   server-side dedup, durable install); the transactional mark-then-claim GC (claim →
//!   unlink-outside-any-transaction → finalize; keep-set = exactly the read-authorization surface) with a
//!   recovery sweep + a quarantine janitor; and the tombstones denylist. The database leads, the filesystem
//!   trails. The GC pass / recovery sweep / quarantine janitor are public ops
//!   ([`Authority::run_gc`] / [`Authority::run_recovery`] / [`Authority::run_janitor`]) the composing
//!   server MUST schedule (startup + periodic) — this library holds no scheduler, and
//!   [`Authority::workspaces`] enumerates the workspaces the per-workspace pass is driven over. The server
//!   clock is one unit throughout: epoch **milliseconds**.
//! - **The size-routed large-object store (offload).** At migrate a file blob >= a configurable threshold is
//!   physically offloaded to a per-workspace content-addressed side store (`location = large-local`), keyed
//!   by the same `blob_id`; smaller blobs stay in git; a per-blob reject cap fails typed at ingest. Identity
//!   is placement-independent (no pointer files); reads (single-object and whole-bundle render) and the GC
//!   unlink dispatch on `location`, still through the skill-scoped access rule; per-workspace roots ⇒ no
//!   cross-workspace dedup. Backend is the local filesystem (`LocalLargeStore` in `topos-gitstore`).
//!
//! ## Deliberately not here yet
//!
//! The large-object store's S3-compatible remote backend + online backfill (additive, client-invisible), the
//! HTTP surface (these writes are exercised in-process only), real identity/roster/device issuance (the
//! registry is fixture-seeded), at-rest key encryption, and the `purge` verb are later work.

// Layout — the orchestration/db twin convention. Each write domain X splits into two halves:
//   src/X.rs    — the orchestration OUTSIDE the transaction (filesystem work, credential derivation,
//                 candidate assembly; no SQL);
//   src/db/X.rs — the raw-SQL half: the one SERIALIZABLE (`run_serializable!`) write transaction plus its
//                 pool reads (no `sqlx` type ever crosses out of `mod db`).
// The twins today: `enroll` (enrollment issuance), `governance` (the owner-signed governance ops + the
// admin claim), and `set_current` (the pointer-move). `session_read` is the first READ twin — pool reads
// only, no `run_serializable!`, no op_id/`workspace_events`/receipts, mirroring `read_roster`'s posture
// (its `db/session_read.rs` holds the one index query; everything else re-uses `read.rs`'s machinery). Exceptions: `gc`'s SQL lives in `db/lifecycle` (one
// fence, one file); the proposals' orchestration lives in `set_current` (propose/approve are arms of the
// one pointer-move write; `db/proposals` holds their SQL); and `db/receipts` is SQL-half-only (the receipt
// read/insert/replay machinery + terminal-outcome writers both `db/set_current` paths call — no
// orchestration twin).
mod authority;
mod db;
mod enroll;
mod error;
mod governance;
mod id;
mod lineage;
mod read;
mod restore;
mod session_read;
mod session_roster;
mod set_current;
mod signer;
mod upload;

// The object-lifecycle fence: `ingest`/`migrate` drive the publish/propose writes, and the GC pass,
// recovery sweep, and quarantine janitor are exposed as the public `Authority::run_gc`/`run_recovery`/
// `run_janitor` (the composing server owns scheduling — this library holds none, but it now hands the
// composer the handles to schedule).
mod gc;
mod lifecycle;

// The feature-gated `impl Authority` test-fixtures shims (seed roster/device/workspace, drive a real genesis
// publish, tamper a signature) — split out of `authority.rs` so the facade reads as exactly the production
// API. Same gate as the shims always had: the production build never compiles it.
#[cfg(feature = "test-fixtures")]
mod fixtures;

#[cfg(test)]
mod tests;

pub use authority::{Authority, PoolConfig};
pub use enroll::{
    ConfirmOutcome, DeploymentMode, DeviceAuthPoll, DeviceAuthStart, EnrollmentConfig,
    EnrollmentDisclosure, EnrollmentRedeemed, GrantIssued, InviteBootstrap, MintedReadToken,
    PasscodeComplete, PasscodeStart, RedeemOutcome, SessionIntent, VerificationContext,
};
pub use error::{AuthorityError, Result};
pub use governance::{
    ApproveStandupOutcome, CreateInviteOutcome, CreateWorkspaceOutcome, GovernanceOp,
    GovernanceOutcome, GovernanceSignedOp, InviteCreated, MintClaimOutcome, MintedClaim, Role,
    WorkspaceCreated,
};
pub use id::{CommitId, IdError, ObjectId, OpId, Principal, SkillId, WorkspaceId};
pub use lineage::{CandidateCommit, LineageDecision};
pub use read::{CurrentPointer, OpenProposalSummary, ReadScope, VersionFile, VersionMeta};
pub use restore::EpochBumpReport;
pub use session_read::SkillIndexRow;
pub use session_roster::{
    RosterSeat, RosterView, SessionInviteOutcome, SessionInviteRole, SessionRotateOutcome,
};
pub use set_current::{DeviceSignedOp, SetCurrentReceipt};
pub use upload::{CandidateUpload, UploadedFile};

/// The embedded Postgres migration set, exposed for out-of-crate test harnesses (the loopback e2e crates)
/// that provision their own per-test database and migrate it before [`Authority::from_pool`] — so they use
/// the SAME migrations as production without a brittle relative path. **Test-fixtures only** (this is the one
/// place, besides [`Authority::from_pool`], where a `sqlx` type is public, and it is compiled out of every
/// production build).
#[cfg(feature = "test-fixtures")]
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Re-exported for constructing [`UploadedFile`]s — the two regular-file modes the kernel allows.
pub use topos_core::digest::FileMode;
