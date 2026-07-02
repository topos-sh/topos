//! The governance + admin-claim core — the orchestration half (outside the transaction).
//!
//! Split from [`crate::enroll`] (the enrollment issuance orchestration) so the role-gated governance
//! surface — the owner-signed invite mint, the roster/revoke mutations, and the self-host first-boot
//! admin claim — reads on its own. This module models the signed governance ops and the workspace roles
//! and does the work OUTSIDE the one write transaction (parse the op id, derive the deterministic invite
//! link, build the server-trusted inputs); the raw SQL — and the `SERIALIZABLE` (`run_serializable!`)
//! governance/claim transactions — live in [`crate::db`]. The credential derivations (HMAC token mint,
//! sha256 storage form, the server-derived device key id) stay in ONE home, [`crate::enroll`].

use crate::authority::Authority;
use crate::enroll::{RedeemOutcome, device_key_id_for, parse_op_id, sha256_token};
use crate::error::Result;
use crate::id::{Principal, SkillId, WorkspaceId};

// ── governance op modeling (an owned mirror of the kernel's borrowed GovernanceOpKind) ─────────────────

/// An owned mirror of [`topos_core::sign::GovernanceOpKind`], so the [`Authority`](crate::Authority) can
/// carry a governance op across the call/transaction boundary and rebuild the borrowed
/// `GovernanceOpFields` for the signing preimage in-transaction. Each variant carries its full signed
/// parameter set; `Invite` additionally carries the per-skill display `name`s (NOT bound into the preimage —
/// only the skill ids are — so a rename never forks the deterministic invite link).
#[derive(Debug, Clone)]
pub enum GovernanceOp {
    /// Invite principals at `role`, expiring `expires_at` (epoch-ms; `None` = never), pre-offering `skills`.
    Invite {
        /// The role the invitees are granted on the workspace roster.
        role: Role,
        /// The invite expiry (epoch-ms; `None` = never expires).
        expires_at: Option<i64>,
        /// The invited principals (the emails), bound into the preimage as a set.
        emails: Vec<Principal>,
        /// The pre-offered skills, each with an optional display name (the name is NOT in the preimage).
        skills: Vec<(SkillId, Option<String>)>,
    },
    /// Set `target`'s workspace role (owner-only).
    RosterSet {
        /// The role to set.
        role: Role,
        /// The principal whose role is set.
        target: Principal,
    },
    /// Remove `target` from the workspace roster (owner-only).
    RosterRemove {
        /// The principal removed.
        target: Principal,
    },
    /// Revoke a registered device key (owner, or the device's own principal).
    DeviceRevoke {
        /// The id of the device key revoked.
        target_device_key_id: String,
    },
}

impl GovernanceOp {
    /// The audit verb string (`workspace_events.gov_op_type`).
    pub(crate) fn audit_verb(&self) -> &'static str {
        match self {
            GovernanceOp::Invite { .. } => "invite",
            GovernanceOp::RosterSet { .. } => "roster_set",
            GovernanceOp::RosterRemove { .. } => "roster_remove",
            GovernanceOp::DeviceRevoke { .. } => "device_revoke",
        }
    }

    /// The op's `target` for the audit row (the affected principal/device; `None` for an invite — its
    /// targets are the multiple invited emails, recorded in `details` instead).
    pub(crate) fn audit_target(&self) -> Option<&str> {
        match self {
            GovernanceOp::Invite { .. } => None,
            GovernanceOp::RosterSet { target, .. } | GovernanceOp::RosterRemove { target } => {
                Some(target.as_str())
            }
            GovernanceOp::DeviceRevoke {
                target_device_key_id,
            } => Some(target_device_key_id.as_str()),
        }
    }
}

/// A governance request signed by an owner's registered device key. The signature is over the kernel
/// governance frame the plane reconstructs from server-trusted values (the request scope + the typed `op`),
/// so a valid signature IS the binding of this device to this exact op — never a client-claimed authority.
#[derive(Debug, Clone)]
pub struct GovernanceSignedOp {
    /// The id of the **signing** owner's device key (the registry selects the public key by this).
    pub device_key_id: String,
    /// The governance op (its kind + parameter tail are rebuilt into the signing preimage).
    pub op: GovernanceOp,
    /// The raw 64-byte Ed25519 governance-op signature.
    pub signature: [u8; 64],
}

/// The result of creating an invite — the shareable link plus the roster + skills it seeded. Re-derives
/// byte-identically on an `op_id` replay (the link is deterministic; the roster/skills come from the op).
#[derive(Debug, Clone)]
pub struct InviteCreated {
    /// The `/i/<token>` link to share (the plaintext token appears ONLY here — only its sha256 is stored).
    pub link: String,
    /// The principals UPSERTed onto the workspace roster as `invited`.
    pub roster_added: Vec<Principal>,
    /// The skills the invite offers, each with its optional display name.
    pub skills: Vec<(SkillId, Option<String>)>,
}

/// The outcome of [`Authority::create_invite`](crate::Authority::create_invite).
#[derive(Debug, Clone)]
pub enum CreateInviteOutcome {
    /// The invite was created (or replayed).
    Created(InviteCreated),
    /// The request was denied (a uniform denial; the static reason is for server logs, never an oracle).
    Denied(&'static str),
}

/// The outcome of a governance mutation (roster set/remove, device revoke).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GovernanceOutcome {
    /// The mutation applied (or replayed).
    Ok,
    /// The mutation was denied (a uniform denial; the static reason is for server logs).
    Denied(&'static str),
}

/// A workspace-level governance role (the `workspace_member` RBAC roster — DISTINCT from the per-skill read
/// `roster`). `owner` signs invites + roster mutations; `member`/`reviewer` cannot. The `signing_byte` is the
/// `u8` bound into the governance/invite signing frames — one mapping, used on both the sign and verify sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Full governance authority (invite, roster, revoke).
    Owner,
    /// A reviewer (review-gate authority; no governance authority in v0).
    Reviewer,
    /// An ordinary member (no governance authority).
    Member,
}

impl Role {
    /// The stored discriminant (`workspace_member.role`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Reviewer => "reviewer",
            Role::Member => "member",
        }
    }

    /// Parse a stored discriminant. `None` on an unknown value.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Role::Owner),
            "reviewer" => Some(Role::Reviewer),
            "member" => Some(Role::Member),
            _ => None,
        }
    }

    /// The `u8` bound into the invite/governance signing frame (owner = 1, reviewer = 2, member = 3) —
    /// delegating to the kernel's `GovernanceRole`, the single mapping the client signer calls too, so
    /// this signature-preimage input can never fork between the halves.
    #[must_use]
    pub fn signing_byte(self) -> u8 {
        match self {
            Role::Owner => topos_core::sign::GovernanceRole::Owner,
            Role::Reviewer => topos_core::sign::GovernanceRole::Reviewer,
            Role::Member => topos_core::sign::GovernanceRole::Member,
        }
        .signing_byte()
    }
}

/// The server-trusted inputs to a governance transaction (create-invite or a roster/revoke mutation).
pub(crate) struct GovernanceInput<'a> {
    /// The target workspace.
    pub ws: &'a WorkspaceId,
    /// The client-minted op id (the idempotency key).
    pub op_id: &'a str,
    /// The op id's raw 16 bytes (bound into the governance signing frame).
    pub op_id_bytes: [u8; 16],
    /// The signed governance op (the signing device key id + the typed op + the signature).
    pub signed: &'a GovernanceSignedOp,
    /// The server-stamped creation timestamp (governance rows are timestamped by `created_at`, not the clock).
    pub created_at: &'a str,
}

// ── the orchestration ops (the public Authority methods delegate to these) ─────────────────────────────

/// Create an owner-signed invite (the orchestration half of [`Authority::create_invite`]). Derives the
/// deterministic invite token, then runs the one governance transaction (authz + store + roster + receipt).
pub(crate) async fn create_invite(
    authority: &Authority,
    ws: &WorkspaceId,
    op_id: &str,
    signed: &GovernanceSignedOp,
    created_at: &str,
) -> Result<CreateInviteOutcome> {
    let GovernanceOp::Invite {
        role,
        expires_at,
        emails,
        skills,
    } = &signed.op
    else {
        return Ok(CreateInviteOutcome::Denied("op is not an invite"));
    };
    let Some(op_id_bytes) = parse_op_id(op_id) else {
        return Ok(CreateInviteOutcome::Denied("op_id is not a canonical UUID"));
    };

    // Derive the deterministic invite token (so a lost-ack retry re-derives the IDENTICAL link). Bind the
    // op id, workspace, role byte, the sorted+deduped skill-id set, and the expiry.
    let mut ids: Vec<&str> = skills.iter().map(|(id, _)| id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    let joined = ids.join("\n");
    let role_byte = [role.signing_byte()];
    // An absent expiry binds the shared no-expiry sentinel — the same value the client signs and the
    // governance frame encodes (`expires_to_u64(None)`); 0i64 and 0u64 share the identical be-bytes.
    let expires_be = expires_at.map_or(
        topos_core::sign::INVITE_NO_EXPIRY.to_be_bytes(),
        i64::to_be_bytes,
    );
    let token = authority.derive_token(
        b"invite",
        &[
            op_id_bytes.as_slice(),
            ws.as_str().as_bytes(),
            role_byte.as_slice(),
            joined.as_bytes(),
            expires_be.as_slice(),
        ],
    )?;
    let token_sha256 = sha256_token(&token);
    let link = format!("{}/i/{token}", authority.enrollment()?.config.base_url);

    let input = GovernanceInput {
        ws,
        op_id,
        op_id_bytes,
        signed,
        created_at,
    };
    match authority
        .db()
        .create_invite_txn(&input, &token_sha256)
        .await?
    {
        GovernanceOutcome::Ok => Ok(CreateInviteOutcome::Created(InviteCreated {
            link,
            roster_added: emails.clone(),
            skills: skills.clone(),
        })),
        GovernanceOutcome::Denied(reason) => Ok(CreateInviteOutcome::Denied(reason)),
    }
}

/// Run a governance roster/revoke mutation (the shared orchestration half of
/// [`Authority::roster_set`] / [`Authority::roster_remove`] / [`Authority::revoke_device`]). Parses the op
/// id, then runs the one governance transaction (authz + the op-specific role check + the mutation + receipt).
pub(crate) async fn governance_mutation(
    authority: &Authority,
    ws: &WorkspaceId,
    op_id: &str,
    signed: &GovernanceSignedOp,
    created_at: &str,
) -> Result<GovernanceOutcome> {
    let Some(op_id_bytes) = parse_op_id(op_id) else {
        return Ok(GovernanceOutcome::Denied("op_id is not a canonical UUID"));
    };
    let input = GovernanceInput {
        ws,
        op_id,
        op_id_bytes,
        signed,
        created_at,
    };
    authority.db().governance_mutation_txn(&input).await
}

/// Consume a self-host admin-claim token (the orchestration half of [`Authority::admin_claim`]).
pub(crate) async fn admin_claim(
    authority: &Authority,
    claim_token: &str,
    device_public_key: [u8; 32],
    display_name: &str,
    now: i64,
    created_at: &str,
) -> Result<RedeemOutcome> {
    let claim_sha256 = sha256_token(claim_token);
    let server_device_key_id = device_key_id_for(&device_public_key);
    authority
        .db()
        .admin_claim_txn(
            &claim_sha256,
            &server_device_key_id,
            &device_public_key,
            display_name,
            now,
            created_at,
        )
        .await
}
