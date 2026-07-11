//! The access-witness seam — the ONLY way custody consults access/identity/policy.
//!
//! Custody (bytes, versions, pointers, GC) makes no access decision of its own and holds no identity
//! SQL: it declares this trait and consumes it, and the directory implements it. The in-transaction
//! methods take the live write transaction, so every answer reflects the rows AS OF this transaction's
//! serializable snapshot — that is what makes a directory row-write instantly effective against byte
//! ops (a device revoke committed before a promotion is seen by that promotion's `device` lookup and
//! blocks it; no duplicated enforcement, no cache to invalidate). The pool-level methods are the cheap
//! read gates for the read surface (the gate half of the gate/reach split).
//!
//! The dependency direction is enforced by `cargo xtask check-arch`: custody modules never name a
//! directory module path or a directory table — this trait is the whole surface between them.

use sqlx::{Postgres, Transaction};

use crate::db::ReadLane;
use crate::error::Result;
use crate::id::{Principal, SkillId, WorkspaceId};

/// A device resolved from its presented credential (`device_key_id`) — the registry row's facts.
#[derive(Debug, Clone)]
pub(crate) struct DeviceIdentity {
    /// The principal the device is bound to (from the trusted row, never a caller claim).
    pub(crate) principal: Principal,
    /// Whether the device has been revoked (a revoked device fails authentication).
    pub(crate) revoked: bool,
}

/// The directory's answer to "may this session principal drive a review/revert write?" — the
/// three-way outcome the transaction's receipt discipline branches on. The ROLE MATRIX (which seats
/// may act) lives entirely on the directory side; custody only maps each answer to its denial class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionWriteGate {
    /// A confirmed seat with review authority — proceed.
    Authorized,
    /// A confirmed plain member — entitled to the durable typed role denial.
    RoleDenied,
    /// Unproven in this workspace (no seat, merely invited, unknown) — a synthesized denial only,
    /// never persisted (the session recording rule).
    Unproven,
}

/// The directory's answers to custody's access questions. Implemented by the directory over its own
/// tables; consumed by custody's transactions and read paths. Every method is a QUESTION (or, for
/// [`seat_roster`](Self::seat_roster), the one directory write the genesis pointer-move must make
/// atomically) — policy semantics stay on the directory side of the seam.
pub(crate) trait AccessWitness {
    /// Resolve a presented device credential to its registry row, inside the live transaction.
    /// `None` ⇒ no such device (indistinguishable from revoked at the caller's surface).
    async fn device(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        device_key_id: &str,
    ) -> Result<Option<DeviceIdentity>>;

    /// Whether the principal holds a per-skill roster row (the device lane's write gate).
    async fn rostered(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
    ) -> Result<bool>;

    /// Whether the principal is a CONFIRMED workspace member (the genesis-standup gate).
    async fn confirmed_member(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        principal: &Principal,
    ) -> Result<bool>;

    /// The session lane's write gate — the directory's role matrix, answered as the three-way outcome
    /// custody's receipt discipline needs (who may review/revert; who is entitled to a durable typed
    /// role denial; who gets only a synthesized one).
    async fn session_write_gate(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        principal: &Principal,
    ) -> Result<SessionWriteGate>;

    /// Whether the workspace's review-required policy is on (read inside the transaction — the
    /// authoritative read; any preflight read is advisory).
    async fn review_required(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
    ) -> Result<bool>;

    /// Seat the principal on the skill's roster — the genesis self-seat, the ONE directory write the
    /// pointer-move performs (atomically with the genesis pointer, so no orphan row can outlive a
    /// rolled-back genesis). Idempotent.
    async fn seat_roster(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
    ) -> Result<()>;

    /// The pool-level principal gate for reads, dispatched by [`ReadLane`] — the gate half of the
    /// gate/reach split (custody owns the lane-blind reachability statements).
    async fn read_gate(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
        lane: ReadLane,
    ) -> Result<bool>;
}
