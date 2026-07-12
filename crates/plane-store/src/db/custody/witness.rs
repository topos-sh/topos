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

use crate::error::Result;
use crate::id::{CommitId, Principal, SkillId, WorkspaceId};

/// A device resolved from its presented workspace credential — the registry row's facts.
#[derive(Debug, Clone)]
pub(crate) struct DeviceIdentity {
    /// The device's stable name (the server-derived `dk_…` id) — the receipts/audit actor. From the
    /// trusted row, never a caller claim (the caller presents only the credential).
    pub(crate) device_key_id: String,
    /// The principal the device is bound to (from the trusted row, never a caller claim).
    pub(crate) principal: Principal,
    /// Whether the device has been revoked. A revoked row still RESOLVES (so a since-revoked device's
    /// lost-ack retry can replay its stored receipt); the caller separately denies fresh work on it.
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

/// A confirmed member's role band, as custody consumes it (the string vocabulary stays on the
/// directory side). The bands decide who a protection gate downgrades: a `Member`'s direct publish
/// on a reviewed bundle becomes a proposal; `Reviewer`/`Owner` land directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActorRole {
    Member,
    Reviewer,
    Owner,
}

impl ActorRole {
    /// Whether this role lands directly on a `reviewed` bundle (the protected-branch model:
    /// reviewers and owners bypass the downgrade; members' publishes become proposals).
    pub(crate) fn lands_on_reviewed(self) -> bool {
        matches!(self, Self::Reviewer | Self::Owner)
    }
}

/// The catalog's answer for a skill a write is about to touch: its lifecycle status + the resolved
/// per-bundle protection (the per-bundle pin, else the workspace default — the cascade lives on the
/// directory side; custody sees only the resolved boolean).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillGate {
    /// No catalog row yet — a genesis, or a pre-catalog seeded pointer (the registration below
    /// self-heals it). The workspace default still answers the protection question.
    Missing { reviewed: bool },
    /// In circulation.
    Active { reviewed: bool },
    /// Archived — out of circulation, not out of history: every pointer write is refused typed.
    Archived,
    /// Deleted — a tombstone; every pointer write is refused typed.
    Deleted,
}

impl SkillGate {
    /// The resolved protection for gating a write (archived/deleted never reach the gate — their
    /// arms refuse first).
    pub(crate) fn reviewed(self) -> bool {
        match self {
            Self::Missing { reviewed } | Self::Active { reviewed } => reviewed,
            Self::Archived | Self::Deleted => false,
        }
    }
}

/// The outcome of placing a skill reference into a channel (`publish --to`, or the `everyone`
/// default at genesis) — the directory's curation policy, answered for the receipt's detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlacementDecision {
    /// Placed into the existing channel.
    Placed { channel: String },
    /// The channel did not exist; created (member-level self-serve) and placed.
    Created { channel: String },
    /// The channel is `curated` and the actor's role is below reviewer — the placement is refused;
    /// the publish itself still stands (the channel mode gates curation independently of the
    /// version gate).
    RoleDenied { channel: String },
    /// The channel name violates the charset (placement refused; the publish stands).
    BadName { channel: String },
}

/// The outcome of registering a skill in the catalog at its first publish (or self-healing a
/// pre-catalog pointer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GenesisRegistration {
    /// Registered (or already present) under this catalog name; the placement decision for the
    /// requested channel (or the `everyone` default) rides along.
    Registered {
        name: String,
        placement: PlacementDecision,
    },
    /// The derived name is already taken by a DIFFERENT skill — the publish is refused typed (two
    /// identities cannot share one name; the author renames their folder or follows the existing
    /// skill).
    NameTaken { name: String },
}

/// The directory's answers to custody's access questions. Implemented by the directory over its own
/// tables; consumed by custody's transactions and read paths. Every method is a QUESTION or one of
/// the few directory writes the pointer-move must make atomically (catalog registration, channel
/// placement, the advisory display name, verdict notices) — policy semantics stay on the directory
/// side of the seam.
pub(crate) trait AccessWitness {
    /// Resolve a presented workspace credential (by its sha256) to its registry row, inside the live
    /// transaction. The lookup IS the authentication. `None` ⇒ no such credential (an unknown
    /// credential is indistinguishable from a rotated-away one at the caller's surface); a REVOKED
    /// row still resolves — the caller checks [`DeviceIdentity::revoked`] after its replay probe.
    async fn device(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        credential_sha256: &[u8; 32],
    ) -> Result<Option<DeviceIdentity>>;

    /// Whether the principal is a CONFIRMED workspace member — the reject transaction's write gate
    /// (and the standup doors'): membership is the ONE authorization predicate on every lane.
    async fn confirmed_member(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        principal: &Principal,
    ) -> Result<bool>;

    /// The principal's confirmed role band, or `None` for no confirmed seat — the device lane's
    /// write gate AND the protection gate's input (a reviewer's direct publish lands on a reviewed
    /// bundle; a member's downgrades to a proposal).
    async fn member_role(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        principal: &Principal,
    ) -> Result<Option<ActorRole>>;

    /// The session lane's write gate — the directory's role matrix, answered as the three-way outcome
    /// custody's receipt discipline needs (who may review/revert; who is entitled to a durable typed
    /// role denial; who gets only a synthesized one).
    async fn session_write_gate(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        principal: &Principal,
    ) -> Result<SessionWriteGate>;

    /// The skill's catalog gate: lifecycle status + resolved protection (per-bundle pin, else the
    /// workspace default), read inside the transaction — the authoritative policy read for the
    /// downgrade decision, the four-eyes trigger, and the archived/deleted refusals.
    async fn skill_gate(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        skill: &SkillId,
    ) -> Result<SkillGate>;

    /// Register a skill in the catalog at its first publish — the genesis directory writes, atomic
    /// with the pointer: the catalog row (name minted from the advisory display name, else the skill
    /// id), the structural `everyone` channel, the placement (the requested `--to` channel, else
    /// `everyone`), and the author's self-follow (an author follows what they create). Idempotent
    /// for an existing registration (then only the placement/display-name halves apply).
    #[allow(clippy::too_many_arguments)]
    async fn register_publish(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        skill: &SkillId,
        display_name: Option<&str>,
        author: &Principal,
        to_channel: Option<&str>,
        created_at: &str,
    ) -> Result<GenesisRegistration>;

    /// Place a skill reference into a channel (`publish --to` on an already-registered skill) —
    /// creating the channel on first use; the channel's mode gates it (open → member, curated →
    /// reviewer+), independently of the version gate.
    async fn place_skill(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        skill: &SkillId,
        channel: &str,
        actor: &Principal,
        created_at: &str,
    ) -> Result<PlacementDecision>;

    /// Record the skill's advisory display name on the catalog (last-writer-wins among writers that
    /// express one; never part of any digest).
    async fn set_display_name(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        skill: &SkillId,
        display_name: &str,
    ) -> Result<()>;

    /// Emit a person-scoped verdict notice (approve/reject carrying its reason) to the proposal's
    /// author — written inside the deciding transaction so a verdict and its notice commit together.
    #[allow(clippy::too_many_arguments)]
    async fn notify_verdict(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        skill: &SkillId,
        version: CommitId,
        recipient: &Principal,
        outcome: &str,
        reason: Option<&str>,
        actor: &Principal,
        created_at: &str,
    ) -> Result<()>;

    /// The pool-level principal gate for reads — a CONFIRMED `workspace_member` row exists. The gate
    /// half of the gate/reach split (custody owns the lane-blind reachability statements); the ONE
    /// membership predicate, shared by the device and session lanes (the lanes differ only in how
    /// the principal was authenticated — presented credential vs. verified session).
    async fn read_gate(&self, ws: &WorkspaceId, principal: &Principal) -> Result<bool>;
}
