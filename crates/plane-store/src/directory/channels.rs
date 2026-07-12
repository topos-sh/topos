//! Channels — the device-lane curation / membership / subscription / protection ops (the
//! orchestration half; the raw SQL + the guarded `topos_*` function calls live in
//! `db/directory/channels.rs`).
//!
//! Every op here is authenticated by the ONE workspace credential (the presented bearer's sha256
//! resolved to its non-revoked registry row — the lookup IS the authentication) and front-doored by
//! the ONE membership predicate (a CONFIRMED `workspace_member` seat), exactly like the device read
//! lane: every pre-gate miss — unknown/revoked credential, non-member, unknown workspace, unknown
//! name — is the single indistinguishable [`AuthorityError::NotFound`]. Past the front door, the
//! ROLE and MODE gates live in the guarded SQL functions (curated channels need reviewer+;
//! loosening protection needs an owner; `everyone` is structural), answered as the typed outcomes
//! below — an authenticated member is entitled to the real reason.
//!
//! These ops are naturally idempotent row writes (place/join/follow re-runs converge) — no op-id
//! receipt machinery; the channel audit trail is trigger-emitted on the underlying writes, so no
//! caller can skip it. Skills are addressed by their user-facing catalog NAME (resolution to the
//! immutable skill id happens in-transaction — id-keyed references are what make rename-on-archive
//! safe); channels by their name.

use crate::Authority;
use crate::db::custody::witness::{AccessWitness, DeviceIdentity};
use crate::error::{AuthorityError, Result};
use crate::id::{Principal, WorkspaceId};

/// A curation write's outcome (`channel add` / `channel remove` / `publish --to`'s standalone twin).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurationOutcome {
    /// The reference was placed into the existing channel.
    Placed,
    /// The channel did not exist: created (member-level self-serve, same rule as `publish --to`)
    /// and the reference placed.
    Created,
    /// The reference was removed.
    Removed,
    /// Nothing to remove — the skill was not in the channel (idempotent information, not an error).
    NotPlaced,
    /// The channel is `curated` and the actor is a plain member (curation there takes reviewer+).
    CuratedRoleRequired,
    /// The (new) channel name violates the charset (lowercase letters, digits, hyphens).
    BadName,
    /// The skill is archived/deleted — out of circulation; placements are refused typed.
    SkillNotActive,
}

/// A self-serve channel membership change's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelMembershipOutcome {
    Joined,
    Left,
    /// Nothing to leave — the person was not a member (idempotent information).
    NotMember,
    /// `everyone` is structural: it mirrors the roster and cannot be joined or left.
    Builtin,
}

/// A person-scoped subscription write's outcome (`follow` / `unfollow` / the device exclusion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionOutcome {
    Followed,
    Unfollowed,
    Excluded,
    /// The skill is archived — a freed name is a NEW identity; the old one refuses follows.
    SkillNotActive,
}

/// The `protect` setter's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtectOutcome {
    Set,
    /// Tightening (→ `reviewed` / `curated`) takes reviewer+.
    ReviewerRoleRequired,
    /// Loosening back to `open` widens what members can do — an owner act.
    OwnerRoleRequired,
}

/// Which kind of thing a `protect` targets (each kind has its own protected level).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectKind {
    Skill,
    Channel,
}

/// The `protect` level, kind-polymorphic: `Protected` = `reviewed` for a skill, `curated` for a
/// channel; `Open` loosens either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectLevel {
    Open,
    Protected,
}

impl ProtectLevel {
    pub(crate) fn skill_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Protected => "reviewed",
        }
    }
    pub(crate) fn channel_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Protected => "curated",
        }
    }
}

/// One channel of the workspace, as `channels_index` returns it: identity + mode + whether the
/// caller belongs, the member count, and the skill references it holds (both name-sorted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelIndexEntry {
    /// The channel's user-facing name (`everyone` for the structural builtin).
    pub name: String,
    /// `"open"` / `"curated"` — the mode the curation gate reads.
    pub mode: String,
    /// The structural `everyone` (roster-derived membership; unjoinable/unleavable).
    pub builtin: bool,
    /// Whether the caller belongs (always true for `builtin`, else a `channel_members` row exists).
    pub member: bool,
    /// The member count: the confirmed-roster size for `builtin`, else the `channel_members` count.
    pub member_count: u64,
    /// The skill references the channel holds, name-sorted.
    pub skills: Vec<ChannelSkillRef>,
}

/// One skill reference held by a channel — the immutable custody id + the current catalog name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSkillRef {
    pub skill_id: String,
    pub name: String,
}

/// The channels index (`channel` bare read): every channel of the workspace — `everyone` included,
/// name-sorted — with the caller's membership, the member count, and the name-sorted skill
/// references. Device-lane authenticated + front-doored by the ONE membership predicate; a miss is
/// the uniform [`AuthorityError::NotFound`].
pub(crate) async fn channels_index(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
) -> Result<Vec<ChannelIndexEntry>> {
    let identity = device_member(authority, ws, credential).await?;
    authority.db().channels_index(ws, &identity.principal).await
}

/// The shared device-lane front door: credential sha256 → non-revoked registry row → CONFIRMED
/// membership. Every miss is the caller's uniform `NotFound`. `pub(crate)` so the sibling `describe`
/// ops share the one front door rather than re-deriving it.
pub(crate) async fn device_member(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
) -> Result<DeviceIdentity> {
    let credential_sha256 = topos_core::digest::sha256(credential.as_bytes());
    let identity = authority
        .db()
        .resolve_read_credential(ws, &credential_sha256)
        .await?
        .ok_or(AuthorityError::NotFound)?;
    if !authority.db().read_gate(ws, &identity.principal).await? {
        return Err(AuthorityError::NotFound);
    }
    Ok(identity)
}

pub(crate) async fn channel_place(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
    channel: &str,
    skill_name: &str,
    created_at: &str,
) -> Result<CurationOutcome> {
    let identity = device_member(authority, ws, credential).await?;
    authority
        .db()
        .channel_place_txn(ws, channel, skill_name, &identity.principal, created_at)
        .await
}

pub(crate) async fn channel_unplace(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
    channel: &str,
    skill_name: &str,
    created_at: &str,
) -> Result<CurationOutcome> {
    let identity = device_member(authority, ws, credential).await?;
    authority
        .db()
        .channel_unplace_txn(ws, channel, skill_name, &identity.principal, created_at)
        .await
}

pub(crate) async fn channel_join(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
    channel: &str,
    created_at: &str,
) -> Result<ChannelMembershipOutcome> {
    let identity = device_member(authority, ws, credential).await?;
    authority
        .db()
        .channel_join_txn(ws, channel, &identity.principal, created_at)
        .await
}

pub(crate) async fn channel_leave(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
    channel: &str,
    now: i64,
    created_at: &str,
) -> Result<ChannelMembershipOutcome> {
    let identity = device_member(authority, ws, credential).await?;
    authority
        .db()
        .channel_leave_txn(ws, channel, &identity.principal, now, created_at)
        .await
}

pub(crate) async fn follow_skill(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
    skill_name: &str,
    created_at: &str,
) -> Result<SubscriptionOutcome> {
    let identity = device_member(authority, ws, credential).await?;
    authority
        .db()
        .follow_skill_txn(
            ws,
            skill_name,
            &identity.principal,
            &identity.device_key_id,
            created_at,
        )
        .await
}

pub(crate) async fn unfollow_skill(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
    skill_name: &str,
    now: i64,
    created_at: &str,
) -> Result<SubscriptionOutcome> {
    let identity = device_member(authority, ws, credential).await?;
    authority
        .db()
        .unfollow_skill_txn(ws, skill_name, &identity.principal, now, created_at)
        .await
}

pub(crate) async fn exclude_device(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
    skill_name: &str,
    created_at: &str,
) -> Result<SubscriptionOutcome> {
    let identity = device_member(authority, ws, credential).await?;
    authority
        .db()
        .exclude_device_txn(ws, skill_name, &identity.device_key_id, created_at)
        .await
}

pub(crate) async fn protect(
    authority: &Authority,
    ws: &WorkspaceId,
    credential: &str,
    kind: ProtectKind,
    target_name: &str,
    level: ProtectLevel,
    created_at: &str,
) -> Result<ProtectOutcome> {
    let identity = device_member(authority, ws, credential).await?;
    authority
        .db()
        .protect_txn(
            ws,
            kind,
            target_name,
            level,
            &identity.principal,
            created_at,
        )
        .await
}

/// The person-scoped variants the SESSION tier (a hosted composition's pages) calls with an
/// already-verified email instead of a credential — the same guarded functions, the same membership
/// front door; self-host is NOT denied here (unlike the roster/review session legs) because these
/// are the web app's row ops and the policy functions are the contract. Deliberately
/// minimal: join/leave only (the web "join channel" button); curation and protection stay on the
/// device lane until the web surfaces land.
pub(crate) async fn channel_join_session(
    authority: &Authority,
    ws: &WorkspaceId,
    acting_email: &str,
    channel: &str,
    created_at: &str,
) -> Result<ChannelMembershipOutcome> {
    let acting = Principal::parse(acting_email).map_err(|_| AuthorityError::NotFound)?;
    if !authority.db().read_gate(ws, &acting).await? {
        return Err(AuthorityError::NotFound);
    }
    authority
        .db()
        .channel_join_txn(ws, channel, &acting, created_at)
        .await
}

pub(crate) async fn channel_leave_session(
    authority: &Authority,
    ws: &WorkspaceId,
    acting_email: &str,
    channel: &str,
    now: i64,
    created_at: &str,
) -> Result<ChannelMembershipOutcome> {
    let acting = Principal::parse(acting_email).map_err(|_| AuthorityError::NotFound)?;
    if !authority.db().read_gate(ws, &acting).await? {
        return Err(AuthorityError::NotFound);
    }
    authority
        .db()
        .channel_leave_txn(ws, channel, &acting, now, created_at)
        .await
}
